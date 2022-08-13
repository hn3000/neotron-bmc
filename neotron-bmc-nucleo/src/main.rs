#![no_main]
#![no_std]

///! Neotron BMC Firmware
///!
///! This is the firmware for the Neotron Board Management Controller (BMC). It controls the power, reset, UART and PS/2 ports on a Neotron mainboard.
///! For more details, see the `README.md` file.
///!
///! # Licence
///! This source code as a whole is licensed under the GPL v3. Third-party crates are covered by their respective licences.
use cortex_m::interrupt::free as disable_interrupts;
use heapless::spsc::{Consumer, Producer, Queue};
use rtic::app;
use stm32f4xx_hal::{
	// Those pins are not used atm:
	// PC3:  IRQ_nHOST
	// PA4:  SPI1_NSS
	// PA5:  SPI1_SCK
	// PA6:  SPI1_MISO
	// PA7:  SPI1_MOSI
	// PA13: SWDIO
	// PA14: SWCLK
	// PB6:  I2C1_SCL
	// PB7:  I2C1_SDA
	// PB13: MON_3V3
	// PB13: MON_5V
	gpio::gpioa::{PA10, PA11, PA12, PA13, PA14, PA4, PA5, PA6, PA7, PA9},
	gpio::gpiob::{PB0, PB1, PB12, PB13, PB14, PB15, PB2, PB6, PB7},
	gpio::gpioc::{PC0, PC1, PC13, PC2, PC3, PC5},
	gpio::{Alternate, Edge, Floating, Input, Output, PullUp, PushPull},
	pac,
	prelude::*,
	serial,
};

use neotron_bmc_nucleo as _;
//use neotron_bmc_nucleo::monotonic::{Tim3Monotonic, U16Ext};
use neotron_bmc_nucleo::monotonic::MonoTimer;

/// Version string auto-generated by git.
static VERSION: &'static str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

/// At what rate do we blink the status LED when we're running?
const LED_PERIOD_MS: u32 = 1000;

/// How often we poll the power and reset buttons in milliseconds.
const DEBOUNCE_POLL_INTERVAL_MS: u32 = 75;

/// The states we can be in controlling the DC power
#[derive(Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum DcPowerState {
	/// We've just enabled the DC power (so ignore any incoming long presses!)
	Starting = 1,
	/// We are now fully on. Look for a long press to turn off.
	On = 2,
	/// We are fully off.
	Off = 0,
}

/// Handles decoding incoming PS/2 packets
///
/// Each packet has 11 bits:
///
/// * Start Bit
/// * 8 Data Bits (LSB first)
/// * Parity Bit
/// * Stop Bit
#[derive(Debug)]
pub struct Ps2Decoder {
	bit_mask: u16,
	collector: u16,
}

/// This is our system state, as accessible via SPI reads and writes.
#[derive(Debug)]
pub struct RegisterState {
	firmware_version: &'static str,
}

#[app(device = crate::pac, peripherals = true, dispatchers=[USART2])]
mod app {

	use super::*;
	use fugit::ExtU32;

	#[shared]
	struct Shared {
		/// The power LED CN6, pin 5
		#[lock_free]
		led_power: PC1<Output<PushPull>>,
		/// The status LED CN7, pin 35
		#[lock_free]
		led_status: PC2<Output<PushPull>>,
		/// The FTDI UART header (J105) PA9: CN10, pin 21 and PA10: CN10, pin 33
		#[lock_free]
		serial: serial::Serial<
			pac::USART1,
			(PA9<Alternate<PushPull, 7>>, PA10<Alternate<PushPull, 7>>),
		>,
		/// The Clear-To-Send line on the FTDI UART header
		/// (which the serial object can't handle) CN10, pin 14
		#[lock_free]
		pin_uart_cts: PA11<Alternate<PushPull, 7>>,
		/// The Ready-To-Receive line on the FTDI UART header
		/// (which the serial object can't handle) CN10, pin 12
		#[lock_free]
		pin_uart_rts: PA12<Alternate<PushPull, 7>>,
		/// The power button, CN7, pin 23
		#[lock_free]
		button_power: PC13<Input<PullUp>>,
		/// The reset button, CN10, pin 16
		#[lock_free]
		button_reset: PB12<Input<PullUp>>,
		/// Tracks DC power state
		#[lock_free]
		state_dc_power_enabled: DcPowerState,
		/// Controls the DC-DC PSU, CN8, PIN 6
		#[lock_free]
		pin_dc_on: PC0<Output<PushPull>>,
		/// Controls the Reset signal across the main board, putting all the
		/// chips (except this BMC!) in reset when pulled low. CN10, pin 26
		#[lock_free]
		pin_sys_reset: PB15<Output<PushPull>>,
		/// Clock pin for PS/2 Keyboard port, CN10, pin 6
		#[lock_free]
		ps2_clk0: PC5<Input<Floating>>,
		/// Clock pin for PS/2 Mouse port, CN8, pin 4
		#[lock_free]
		ps2_clk1: PB0<Input<Floating>>,
		/// Data pin for PS/2 Keyboard port, CN10, pin 24
		#[lock_free]
		ps2_dat0: PB1<Input<Floating>>,
		/// Data pin for PS/2 Mouse port, CN10, port 22
		#[lock_free]
		ps2_dat1: PB2<Input<Floating>>,
		/// The external interrupt peripheral
		#[lock_free]
		exti: pac::EXTI,
		/// Our register state
		#[lock_free]
		register_state: RegisterState,

		/// Mouse PS/2 decoder
		ms_decoder: Ps2Decoder,
		/// Keyboard bytes sink
		#[lock_free]
		kb_q_out: Consumer<'static, u16, 8>,
		/// Keyboard bytes source
		#[lock_free]
		kb_q_in: Producer<'static, u16, 8>,
	}

	#[local]
	struct Local {
		/// Tracks power button state for short presses.
		/// 75ms x 2 = 150ms is a short press : DOES THE COMMENT MATCH?
		press_button_power_short: debouncr::Debouncer<u8, debouncr::Repeat2>,
		/// Tracks power button state for long presses.
		/// 75ms x 16 = 1200ms is a long press
		press_button_power_long: debouncr::Debouncer<u16, debouncr::Repeat16>,
		/// Tracks reset button state for long presses. 75ms x 16 = 1200ms is a long press
		press_button_reset_long: debouncr::Debouncer<u16, debouncr::Repeat16>,
		/// Keyboard PS/2 decoder
		kb_decoder: Ps2Decoder,
	}

	#[monotonic(binds = TIM2, default = true)]
	type MyMono = MonoTimer<pac::TIM2, 1_000_000>;

	/// The entry point to our application.
	///
	/// Sets up the hardware and spawns the regular tasks.
	///
	/// * Task `led_power_blink` - blinks the LED
	/// * Task `button_poll` - checks the power and reset buttons
	#[init(local = [queue: Queue<u16, 8> = Queue::new()])]
	fn init(ctx: init::Context) -> (Shared, Local, init::Monotonics) {
		//static mut Q: Queue<u16, U8> = Queue(i::Queue::new());
		defmt::info!("Neotron BMC Nucleo version {:?} booting", VERSION);

		let mut dp: pac::Peripherals = ctx.device;

		// FLASH is unused?
		// let mut flash = dp.FLASH;
		// Changed from RCC
		let clocks = dp
			.RCC
			.constrain()
			.cfgr
			.sysclk(84.mhz())
			.hclk(84.mhz())
			//pclk is replaced by pclk1
			.pclk1(42.mhz())
			.freeze();

		defmt::info!("Configuring TIM2...");
		let mono = MyMono::new(dp.TIM2, &clocks);

		defmt::info!("Creating pins...");
		let gpioa = dp.GPIOA.split();
		let gpiob = dp.GPIOB.split();
		let gpioc = dp.GPIOC.split();
		let (
			uart_tx,
			uart_rx,
			pin_uart_cts,
			pin_uart_rts,
			mut led_power,
			mut led_status,
			button_power,
			button_reset,
			mut pin_dc_on,
			mut pin_sys_reset,
			mut ps2_clk0,
			ps2_clk1,
			ps2_dat0,
			ps2_dat1,
		) = disable_interrupts(|_cs| {
			(
				gpioa.pa9.into_alternate(),
				gpioa.pa10.into_alternate(),
				gpioa.pa11.into_alternate(),
				gpioa.pa12.into_alternate(),
				// power led
				gpioc.pc1.into_push_pull_output(),
				// status led
				gpioc.pc2.into_push_pull_output(),
				// power button
				gpioc.pc13.into_pull_up_input(),
				// reset button
				gpiob.pb12.into_pull_up_input(),
				// dc on
				gpioc.pc0.into_push_pull_output(),
				// system reset
				gpiob.pb15.into_push_pull_output(),
				// PS2_CLK0
				gpioc.pc5.into_floating_input(),
				// PS2_CLK1
				gpiob.pb0.into_floating_input(),
				// PS2_DAT0
				gpiob.pb1.into_floating_input(),
				// PS2_DAT1
				gpiob.pb2.into_floating_input(),
			)
		});

		// not returning result anymore
		pin_sys_reset.set_low();
		pin_dc_on.set_low();

		defmt::info!("Creating UART...");

		// let mut serial =
		// 	serial::Serial::usart1(dp.USART1, (uart_tx, uart_rx), 115_200.bps(), &mut rcc);
		// source : https://github.com/jamesmunns/pretty-hal-machine/blob/7f2f50c8c841c6d936a7147a092ec67bbb2602fa/firmware/blackpill-phm/src/main.rs#L103

		let mut serial = serial::Serial::new(
			dp.USART1,
			(uart_tx, uart_rx),
			serial::config::Config::default().baudrate(115_200.bps()),
			&clocks,
		)
		.unwrap();
		serial.listen(serial::Event::Rxne);

		// no result
		led_power.set_low();
		led_status.set_low();

		// Set EXTI15 to use PORT A (PA15)
		// 	PA15	PS2_CLK0	Keyboard Clock Input is now PC5
		// source: https://github.com/kalkyl/f411-rtic/blob/a696fce7d6d19fda2356c37642c4d53547982cca/src/bin/exti.rs#L37-L39
		let mut sys_cfg = dp.SYSCFG.constrain();
		ps2_clk0.make_interrupt_source(&mut sys_cfg);
		ps2_clk0.enable_interrupt(&mut dp.EXTI);
		ps2_clk0.trigger_on_edge(&mut dp.EXTI, Edge::Falling);
		//dp.SYSCFG.exticr4.write(|w| w.exti15().pa15());

		// Enable EXTI15 interrupt as external falling edge
		// dp.EXTI.imr.modify(|_r, w| w.mr15().set_bit());
		// dp.EXTI.emr.modify(|_r, w| w.mr15().set_bit());
		// dp.EXTI.ftsr.modify(|_r, w| w.tr15().set_bit());

		// Spawn the tasks that run all the time
		led_power_blink::spawn().unwrap();
		button_poll::spawn().unwrap();

		defmt::info!("Init complete!");

		let (kb_q_in, kb_q_out) = ctx.local.queue.split();

		let shared_resources = Shared {
			serial,
			pin_uart_cts,
			pin_uart_rts,
			led_power,
			led_status,
			button_power,
			button_reset,
			state_dc_power_enabled: DcPowerState::Off,
			pin_dc_on,
			pin_sys_reset,
			ps2_clk0,
			ps2_clk1,
			ps2_dat0,
			ps2_dat1,
			exti: dp.EXTI,
			register_state: RegisterState {
				firmware_version: "Neotron BMC Nucleo v0.0.0",
			},
			kb_q_in,
			kb_q_out,
			ms_decoder: Ps2Decoder::new(),
		};

		let local_resources = Local {
			press_button_power_short: debouncr::debounce_2(false),
			press_button_power_long: debouncr::debounce_16(false),
			press_button_reset_long: debouncr::debounce_16(false),
			kb_decoder: Ps2Decoder::new(),
		};
		let init = init::Monotonics(mono);

		(shared_resources, local_resources, init)
	}

	/// Our idle task.
	///
	/// This task is called when there is nothing else to do. We
	/// do a little logging, then put the CPU to sleep waiting for an interrupt.
	#[idle(shared = [kb_q_out])]
	fn idle(ctx: idle::Context) -> ! {
		defmt::info!("Idle is running...");
		loop {
			if let Some(word) = ctx.shared.kb_q_out.dequeue() {
				if let Some(byte) = Ps2Decoder::check_word(word) {
					defmt::info!("< KB {:x}", byte);
				} else {
					defmt::info!("< Bad KB {:x}", word);
				}
			}
		}
	}

	/// This is the PS/2 Keyboard task.
	///
	/// It is very high priority, as we can't afford to miss a clock edge.
	///
	/// It fires when there is a falling edge on the PS/2 Keyboard clock pin.
	#[task(
		//apparently it should be EXTI1 is
		binds = EXTI1,
		priority = 4,
		shared=[ps2_clk0, ps2_dat0, exti, kb_q_in],
		local=[kb_decoder]
	)]
	fn exti1_interrupt(ctx: exti1_interrupt::Context) {
		// no result
		let data_bit = ctx.shared.ps2_dat0.is_high();
		// Do we have a complete word (and if so, is the parity OK)?
		if let Some(data) = ctx.local.kb_decoder.add_bit(data_bit) {
			// Don't dump in the ISR - we're busy. Add it to this nice lockless queue instead.
			ctx.shared.kb_q_in.enqueue(data).unwrap();
		}
		// Clear the pending flag
		//ctx.shared.exti.pr.write(|w| w.pr15().set_bit());
		ctx.shared.ps2_dat0.clear_interrupt_pending_bit();
	}

	/// This is the USART1 task.
	///
	/// It fires whenever there is new data received on USART1. We should flag to the host
	/// that data is available.
	#[task(binds = USART1, shared=[serial])]
	fn usart1_interrupt(ctx: usart1_interrupt::Context) {
		// Reading the register clears the RX-Not-Empty-Interrupt flag.
		match ctx.shared.serial.read() {
			Ok(b) => {
				defmt::info!("<< UART {:x}", b);
			}
			Err(_) => {
				defmt::warn!("<< UART None?");
			}
		}
	}

	/// This is the LED blink task.
	///
	/// This task is called periodically. We check whether the status LED is currently on or off,
	/// and set it to the opposite. This makes the LED blink.
	#[task(
		shared = [led_power, state_dc_power_enabled],
		local = [led_state:bool = false]
	)]
	fn led_power_blink(ctx: led_power_blink::Context) {
		if *ctx.shared.state_dc_power_enabled == DcPowerState::Off {
			//TODO: Shall that be local?
			//defmt::trace!("blink time {}", ctx.scheduled.counts());
			if *ctx.local.led_state {
				ctx.shared.led_power.set_low();
				*ctx.local.led_state = false;
			} else {
				ctx.shared.led_power.set_high();
				*ctx.local.led_state = true;
			}

			led_power_blink::spawn_after(LED_PERIOD_MS.millis()).unwrap();
		}
	}

	/// This task polls our power and reset buttons.
	///
	/// We poll them rather than setting up an interrupt as we need to debounce them, which involves waiting a short period and checking them again. Given that we have to do that, we might as well not bother with the interrupt.
	#[task(
		shared = [led_power, button_power, button_reset, state_dc_power_enabled, pin_sys_reset, pin_dc_on],
		local = [press_button_power_short, press_button_power_long, press_button_reset_long]
	)]
	fn button_poll(ctx: button_poll::Context) {
		// Poll button
		let pwr_pressed: bool = ctx.shared.button_power.is_low();
		let rst_pressed: bool = ctx.shared.button_reset.is_low();
		// Update state
		let pwr_short_edge = ctx.local.press_button_power_short.update(pwr_pressed);
		let pwr_long_edge = ctx.local.press_button_power_long.update(pwr_pressed);
		let rst_long_edge = ctx.local.press_button_reset_long.update(rst_pressed);

		// Dispatch event
		match (
			pwr_long_edge,
			pwr_short_edge,
			*ctx.shared.state_dc_power_enabled,
		) {
			(None, Some(debouncr::Edge::Rising), DcPowerState::Off) => {
				defmt::info!("Power button pressed whilst off.");
				// Button pressed - power on system
				*ctx.shared.state_dc_power_enabled = DcPowerState::Starting;
				ctx.shared.led_power.set_high();
				defmt::info!("Power on!");
				ctx.shared.pin_dc_on.set_high();
				// TODO: Start monitoring 3.3V and 5.0V rails here
				// TODO: Take system out of reset when 3.3V and 5.0V are good
				ctx.shared.pin_sys_reset.set_high();
			}
			(None, Some(debouncr::Edge::Falling), DcPowerState::Starting) => {
				defmt::info!("Power button released.");
				// Button released after power on
				*ctx.shared.state_dc_power_enabled = DcPowerState::On;
			}
			(Some(debouncr::Edge::Rising), None, DcPowerState::On) => {
				defmt::info!("Power button held whilst on.");
				*ctx.shared.state_dc_power_enabled = DcPowerState::Off;
				ctx.shared.led_power.set_low();
				defmt::info!("Power off!");
				ctx.shared.pin_sys_reset.set_low();
				// TODO: Wait for 100ms for chips to stop?
				ctx.shared.pin_dc_on.set_low();
				// Start LED blinking again
				led_power_blink::spawn();
			}
			_ => {
				// Do nothing
				// TODO: Put system in reset here
				// TODO: Disable DC PSU here
			}
		}

		if let Some(debouncr::Edge::Falling) = rst_long_edge {
			defmt::info!("Reset!");
			ctx.shared.pin_sys_reset.set_low();
			// TODO: This pulse will be very short. We should spawn a task to
			// take it out of reset after about 100ms.
			ctx.shared.pin_sys_reset.set_high();
		}
		// Re-schedule the timer interrupt
		button_poll::spawn_after(DEBOUNCE_POLL_INTERVAL_MS.millis()).unwrap();
	}
}

impl Ps2Decoder {
	fn new() -> Ps2Decoder {
		Ps2Decoder {
			bit_mask: 1,
			collector: 0,
		}
	}

	fn reset(&mut self) {
		self.bit_mask = 0;
		self.collector = 0;
	}

	fn add_bit(&mut self, bit: bool) -> Option<u16> {
		if bit {
			self.collector |= self.bit_mask;
		}
		self.bit_mask <<= 1;
		if self.bit_mask == 0b100000000000 {
			let result = self.collector;
			self.reset();
			Some(result)
		} else {
			None
		}
	}

	/// Check 11-bit word has 1 start bit, 1 stop bit and an odd parity bit.
	fn check_word(word: u16) -> Option<u8> {
		let start_bit = (word & 0x0001) != 0;
		let parity_bit = (word & 0x0200) != 0;
		let stop_bit = (word & 0x0400) != 0;
		let data = ((word >> 1) & 0xFF) as u8;

		if start_bit {
			return None;
		}

		if !stop_bit {
			return None;
		}

		let need_parity = (data.count_ones() % 2) == 0;

		// Odd parity, so these must not match
		if need_parity != parity_bit {
			return None;
		}

		Some(data)
	}
}

// TODO: Pins we haven't used yet
// SPI pins
// spi_clk: gpioa.pa5.into_alternate_af0(cs),
// spi_cipo: gpioa.pa6.into_alternate_af0(cs),
// spi_copi: gpioa.pa7.into_alternate_af0(cs),
// I²C pins
// i2c_scl: gpiob.pb6.into_alternate_af4(cs),
// i2c_sda: gpiob.pb7.into_alternate_af4(cs),
