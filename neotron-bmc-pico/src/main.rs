//! Neotron BMC Firmware
//!
//! This is the firmware for the Neotron Board Management Controller (BMC) as
//! fitted to a Neotron Pico. It controls the power, reset, UART and PS/2 ports
//! on that Neotron mainboard. For more details, see the `README.md` file.
//!
//! # Licence
//! This source code as a whole is licensed under the GPL v3. Third-party crates
//! are covered by their respective licences.

#![no_main]
#![no_std]

use heapless::spsc::{Consumer, Producer, Queue};
use rtic::app;
use stm32f0xx_hal::{
	gpio::gpioa::{PA10, PA11, PA12, PA15, PA2, PA3, PA9},
	gpio::gpiob::{PB0, PB1, PB3, PB4, PB5},
	gpio::gpiof::{PF0, PF1},
	gpio::{Alternate, Floating, Input, Output, PullUp, PushPull, AF1},
	pac,
	prelude::*,
	serial,
};

use neotron_bmc_pico as _;

/// Version string auto-generated by git.
static VERSION: &'static str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

/// At what rate do we blink the status LED when we're running?
const LED_PERIOD_MS: u64 = 1000;

/// How often we poll the power and reset buttons in milliseconds.
const DEBOUNCE_POLL_INTERVAL_MS: u64 = 75;

/// Length of a reset pulse, in milliseconds
const RESET_DURATION_MS: u64 = 250;

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

#[app(device = crate::pac, peripherals = true, dispatchers = [USB, USART3_4_5_6, TIM14, TIM15, TIM16, TIM17, PVD])]
mod app {
	use super::*;
	use systick_monotonic::*; // Implements the `Monotonic` trait

	#[shared]
	struct Shared {
		/// The power LED (D1101)
		#[lock_free]
		led_power: PB0<Output<PushPull>>,
		/// The status LED (D1102)
		#[lock_free]
		_buzzer_pwm: PB1<Output<PushPull>>,
		/// The FTDI UART header (J105)
		#[lock_free]
		serial: serial::Serial<pac::USART1, PA9<Alternate<AF1>>, PA10<Alternate<AF1>>>,
		/// The Clear-To-Send line on the FTDI UART header (which the serial object can't handle)
		#[lock_free]
		_pin_uart_cts: PA11<Alternate<AF1>>,
		/// The Ready-To-Receive line on the FTDI UART header (which the serial object can't handle)
		#[lock_free]
		_pin_uart_rts: PA12<Alternate<AF1>>,
		/// The power button
		#[lock_free]
		button_power: PF0<Input<PullUp>>,
		/// The reset button
		#[lock_free]
		button_reset: PF1<Input<PullUp>>,
		/// Tracks DC power state
		#[lock_free]
		state_dc_power_enabled: DcPowerState,
		/// Controls the DC-DC PSU
		#[lock_free]
		pin_dc_on: PA3<Output<PushPull>>,
		/// Controls the Reset signal across the main board, putting all the
		/// chips (except this BMC!) in reset when pulled low.
		#[lock_free]
		pin_sys_reset: PA2<Output<PushPull>>,
		/// Clock pin for PS/2 Keyboard port
		#[lock_free]
		ps2_clk0: PA15<Input<Floating>>,
		/// Clock pin for PS/2 Mouse port
		#[lock_free]
		_ps2_clk1: PB3<Input<Floating>>,
		/// Data pin for PS/2 Keyboard port
		#[lock_free]
		ps2_dat0: PB4<Input<Floating>>,
		/// Data pin for PS/2 Mouse port
		#[lock_free]
		_ps2_dat1: PB5<Input<Floating>>,
		/// The external interrupt peripheral
		#[lock_free]
		exti: pac::EXTI,
		/// Our register state
		#[lock_free]
		_register_state: RegisterState,
		/// Keyboard words sink
		#[lock_free]
		kb_q_out: Consumer<'static, u16, 8>,
	}

	#[local]
	struct Local {
		/// Tracks power button state for short presses. 75ms x 2 = 150ms is a short press
		press_button_power_short: debouncr::Debouncer<u8, debouncr::Repeat2>,
		/// Tracks power button state for long presses. 75ms x 16 = 1200ms is a long press
		press_button_power_long: debouncr::Debouncer<u16, debouncr::Repeat16>,
		/// Tracks reset button state for short presses. 75ms x 2 = 150ms is a long press
		press_button_reset_short: debouncr::Debouncer<u8, debouncr::Repeat2>,
		/// Keyboard PS/2 decoder
		kb_decoder: Ps2Decoder,
		/// Keyboard words source
		kb_q_in: Producer<'static, u16, 8>,
	}

	#[monotonic(binds = SysTick, default = true)]
	type MyMono = Systick<200>; // 200 Hz (= 5ms) timer tick

	/// The entry point to our application.
	///
	/// Sets up the hardware and spawns the regular tasks.
	///
	/// * Task `led_power_blink` - blinks the LED
	/// * Task `button_poll` - checks the power and reset buttons
	#[init(local = [ queue: Queue<u16, 8> = Queue::new()])]
	fn init(ctx: init::Context) -> (Shared, Local, init::Monotonics) {
		defmt::info!("Neotron BMC version {:?} booting", VERSION);

		let dp: pac::Peripherals = ctx.device;
		let cp: cortex_m::Peripherals = ctx.core;

		let mut flash = dp.FLASH;
		let mut rcc = dp
			.RCC
			.configure()
			.hclk(48.mhz())
			.pclk(48.mhz())
			.sysclk(48.mhz())
			.freeze(&mut flash);

		defmt::info!("Configuring SysTick...");
		// Initialize the monotonic timer using the Cortex-M SysTick peripheral
		let mono = Systick::new(cp.SYST, rcc.clocks.sysclk().0);

		defmt::info!("Creating pins...");
		let gpioa = dp.GPIOA.split(&mut rcc);
		let gpiob = dp.GPIOB.split(&mut rcc);
		let gpiof = dp.GPIOF.split(&mut rcc);
		// We have to have the closure return a tuple of all our configured
		// pins because by taking fields from `gpioa`, `gpiob`, etc, we leave
		// them as partial structures. This prevents us from having a call to
		// `disable_interrupts` for each pin. We can't simply do the `let foo
		// = ` inside the closure either, as the pins would be dropped when
		// the closure ended. So, we have this slightly awkward syntax
		// instead. Do ensure the pins and the variables line-up correctly;
		// order is important!
		let (
			uart_tx,
			uart_rx,
			_pin_uart_cts,
			_pin_uart_rts,
			mut led_power,
			mut _buzzer_pwm,
			button_power,
			button_reset,
			mut pin_dc_on,
			mut pin_sys_reset,
			ps2_clk0,
			_ps2_clk1,
			ps2_dat0,
			_ps2_dat1,
		) = cortex_m::interrupt::free(|cs| {
			(
				gpioa.pa9.into_alternate_af1(cs),
				gpioa.pa10.into_alternate_af1(cs),
				gpioa.pa11.into_alternate_af1(cs),
				gpioa.pa12.into_alternate_af1(cs),
				gpiob.pb0.into_push_pull_output(cs),
				gpiob.pb1.into_push_pull_output(cs),
				gpiof.pf0.into_pull_up_input(cs),
				gpiof.pf1.into_pull_up_input(cs),
				gpioa.pa3.into_push_pull_output(cs),
				gpioa.pa2.into_push_pull_output(cs),
				gpioa.pa15.into_floating_input(cs),
				gpiob.pb3.into_floating_input(cs),
				gpiob.pb4.into_floating_input(cs),
				gpiob.pb5.into_floating_input(cs),
			)
		});

		pin_sys_reset.set_low().unwrap();
		pin_dc_on.set_low().unwrap();

		defmt::info!("Creating UART...");

		let mut serial =
			serial::Serial::usart1(dp.USART1, (uart_tx, uart_rx), 115_200.bps(), &mut rcc);

		serial.listen(serial::Event::Rxne);

		led_power.set_low().unwrap();
		_buzzer_pwm.set_low().unwrap();

		// Set EXTI15 to use PORT A (PA15)
		dp.SYSCFG.exticr4.write(|w| w.exti15().pa15());

		// Enable EXTI15 interrupt as external falling edge
		dp.EXTI.imr.modify(|_r, w| w.mr15().set_bit());
		dp.EXTI.emr.modify(|_r, w| w.mr15().set_bit());
		dp.EXTI.ftsr.modify(|_r, w| w.tr15().set_bit());

		// Spawn the tasks that run all the time
		led_power_blink::spawn().unwrap();
		button_poll::spawn().unwrap();

		defmt::info!("Init complete!");

		let (kb_q_in, kb_q_out) = ctx.local.queue.split();

		let shared_resources = Shared {
			serial,
			_pin_uart_cts,
			_pin_uart_rts,
			led_power,
			_buzzer_pwm,
			button_power,
			button_reset,
			state_dc_power_enabled: DcPowerState::Off,
			pin_dc_on,
			pin_sys_reset,
			ps2_clk0,
			_ps2_clk1,
			ps2_dat0,
			_ps2_dat1,
			exti: dp.EXTI,
			_register_state: RegisterState {
				firmware_version: concat!("Neotron BMC ", env!("CARGO_PKG_VERSION")),
			},
			kb_q_out,
		};
		let local_resources = Local {
			press_button_power_short: debouncr::debounce_2(false),
			press_button_power_long: debouncr::debounce_16(false),
			press_button_reset_short: debouncr::debounce_2(false),
			kb_decoder: Ps2Decoder::new(),
			kb_q_in,
		};
		let init = init::Monotonics(mono);
		(shared_resources, local_resources, init)
	}

	/// Our idle task.
	///
	/// This task is called when there is nothing else to do.
	#[idle(shared = [kb_q_out])]
	fn idle(ctx: idle::Context) -> ! {
		defmt::info!("Idle is running...");
		loop {
			if let Some(word) = ctx.shared.kb_q_out.dequeue() {
				if let Some(byte) = Ps2Decoder::check_word(word) {
					defmt::info!("< KB 0x{:x}", byte);
				} else {
					defmt::warn!("< Bad KB 0x{:x}", word);
				}
			}

			// TODO: Read ADC for 3.3V and 5.0V rails and check good
		}
	}

	/// This is the PS/2 Keyboard task.
	///
	/// It is very high priority, as we can't afford to miss a clock edge.
	///
	/// It fires when there is a falling edge on the PS/2 Keyboard clock pin.
	#[task(
		binds = EXTI4_15,
		priority = 4,
		shared = [ps2_clk0, ps2_dat0, exti],
		local = [kb_decoder, kb_q_in]
	)]
	fn exti4_15_interrupt(ctx: exti4_15_interrupt::Context) {
		let data_bit = ctx.shared.ps2_dat0.is_high().unwrap();
		// Do we have a complete word (and if so, is the parity OK)?
		if let Some(data) = ctx.local.kb_decoder.add_bit(data_bit) {
			// Don't dump in the ISR - we're busy. Add it to this nice lockless queue instead.
			ctx.local.kb_q_in.enqueue(data).unwrap();
		}
		// Clear the pending flag
		ctx.shared.exti.pr.write(|w| w.pr15().set_bit());
	}

	/// This is the USART1 task.
	///
	/// It fires whenever there is new data received on USART1. We should flag to the host
	/// that data is available.
	#[task(binds = USART1, shared = [serial])]
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
	#[task(shared = [led_power, state_dc_power_enabled], local = [ led_state: bool = false ])]
	fn led_power_blink(ctx: led_power_blink::Context) {
		if *ctx.shared.state_dc_power_enabled == DcPowerState::Off {
			if *ctx.local.led_state {
				ctx.shared.led_power.set_low().unwrap();
				*ctx.local.led_state = false;
			} else {
				ctx.shared.led_power.set_high().unwrap();
				*ctx.local.led_state = true;
			}
			led_power_blink::spawn_after(LED_PERIOD_MS.millis()).unwrap();
		}
	}

	/// This task polls our power and reset buttons.
	///
	/// We poll them rather than setting up an interrupt as we need to debounce
	/// them, which involves waiting a short period and checking them again.
	/// Given that we have to do that, we might as well not bother with the
	/// interrupt.
	#[task(
		shared = [
			led_power, button_power, button_reset,
			state_dc_power_enabled, pin_sys_reset, pin_dc_on
		],
		local = [ press_button_power_short, press_button_power_long, press_button_reset_short ]
	)]
	fn button_poll(ctx: button_poll::Context) {
		// Poll buttons
		let pwr_pressed: bool = ctx.shared.button_power.is_low().unwrap();
		let rst_pressed: bool = ctx.shared.button_reset.is_low().unwrap();

		// Update state
		let pwr_short_edge = ctx.local.press_button_power_short.update(pwr_pressed);
		let pwr_long_edge = ctx.local.press_button_power_long.update(pwr_pressed);
		let rst_long_edge = ctx.local.press_button_reset_short.update(rst_pressed);

		defmt::trace!(
			"pwr/rst {}/{} {}",
			pwr_pressed,
			rst_pressed,
			match rst_long_edge {
				Some(debouncr::Edge::Rising) => "rising",
				Some(debouncr::Edge::Falling) => "falling",
				None => "-",
			}
		);

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
				ctx.shared.led_power.set_high().unwrap();
				defmt::info!("Power on!");
				ctx.shared.pin_dc_on.set_high().unwrap();
				// TODO: Start monitoring 3.3V and 5.0V rails here
				// TODO: Take system out of reset when 3.3V and 5.0V are good
				ctx.shared.pin_sys_reset.set_high().unwrap();
			}
			(None, Some(debouncr::Edge::Falling), DcPowerState::Starting) => {
				defmt::info!("Power button released.");
				// Button released after power on
				*ctx.shared.state_dc_power_enabled = DcPowerState::On;
			}
			(Some(debouncr::Edge::Rising), None, DcPowerState::On) => {
				defmt::info!("Power button held whilst on.");
				*ctx.shared.state_dc_power_enabled = DcPowerState::Off;
				ctx.shared.led_power.set_low().unwrap();
				defmt::info!("Power off!");
				ctx.shared.pin_sys_reset.set_low().unwrap();
				ctx.shared.pin_dc_on.set_low().unwrap();
				// Start LED blinking again
				led_power_blink::spawn().unwrap();
			}
			_ => {
				// Do nothing
			}
		}

		// Did reset get a long press?
		if let Some(debouncr::Edge::Rising) = rst_long_edge {
			// Is the board powered on? Don't do a reset if it's powered off.
			if *ctx.shared.state_dc_power_enabled == DcPowerState::On {
				defmt::info!("Reset!");
				ctx.shared.pin_sys_reset.set_low().unwrap();
				// Returns an error if it's already scheduled
				let _ = exit_reset::spawn_after(RESET_DURATION_MS.millis());
			}
		}

		// Re-schedule the timer interrupt
		button_poll::spawn_after(DEBOUNCE_POLL_INTERVAL_MS.millis()).unwrap();
	}

	/// Return the reset line high (inactive), but only if we're still powered on.
	#[task(shared = [pin_sys_reset, state_dc_power_enabled])]
	fn exit_reset(ctx: exit_reset::Context) {
		defmt::debug!("End reset");
		if *ctx.shared.state_dc_power_enabled == DcPowerState::On {
			ctx.shared.pin_sys_reset.set_high().unwrap();
		}
	}
}

impl Ps2Decoder {
	/// Create a new PS/2 Decoder
	const fn new() -> Ps2Decoder {
		Ps2Decoder {
			bit_mask: 1,
			collector: 0,
		}
	}

	/// Reset the PS/2 decoder
	fn reset(&mut self) {
		self.bit_mask = 1;
		self.collector = 0;
	}

	/// Add a bit, and if we have enough, return the 11-bit PS/2 word.
	fn add_bit(&mut self, bit: bool) -> Option<u16> {
		if bit {
			self.collector |= self.bit_mask;
		}
		// Was that the last bit we needed?
		if self.bit_mask == 0b100_0000_0000 {
			let result = self.collector;
			self.reset();
			Some(result)
		} else {
			self.bit_mask <<= 1;
			None
		}
	}

	/// Check 11-bit word has 1 start bit, 1 stop bit and an odd parity bit.
	///
	/// If so, you get back the 8 bit data within the word. Otherwise you get
	/// None.
	fn check_word(word: u16) -> Option<u8> {
		let start_bit = (word & 0b000_0000_0001) != 0;
		let parity_bit = (word & 0b010_0000_0000) != 0;
		let stop_bit = (word & 0b100_0000_0000) != 0;
		let data = ((word >> 1) & 0xFF) as u8;

		if start_bit {
			return None;
		}

		if !stop_bit {
			return None;
		}

		let need_parity = (data.count_ones() % 2) == 0;

		// Check we have the correct parity bit
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
