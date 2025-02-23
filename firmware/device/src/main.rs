// Keyboard firmware
#![no_main]
#![no_std]

use rp2040_panic_usb_boot as _;
use rtic::app;

#[link_section = ".config"]
static LAYOUT: &str = include_str!("layout.txt");

#[app(device = rp_pico::hal::pac,
      peripherals = true,
      dispatchers = [DMA_IRQ_0])]
mod app {
    use defmt::*;
    use defmt_rtt as _;

    use ::layout::*;
    use embedded_hal::{
        digital::v2::{InputPin, OutputPin},
        serial::{Read, Write},
    };
    use frunk::{HCons, HNil};
    use fugit::RateExtU32;
    use keyberon::debounce::Debouncer;
    use keyberon::layout::{CustomEvent, Event};
    use keyberon::matrix::Matrix;
    use nb::block;
    use rp2040_monotonic::Rp2040Monotonic;
    use rp_pico::hal;
    use rp_pico::hal::prelude::*;
    use rp_pico::hal::{gpio::dynpin::DynPin, pio::PIOExt, usb::UsbBus};
    use smart_leds::{SmartLedsWrite, RGB8};
    use usb_device::{
        bus::UsbBusAllocator,
        device::{UsbDeviceBuilder, UsbDeviceState, UsbVidPid},
        UsbError,
    };
    use usbd_human_interface_device::{
        device::{
            consumer::{ConsumerControlInterface, MultipleConsumerReport},
            keyboard::NKROBootKeyboardInterface,
        },
        hid_class::{UsbHidClass, UsbHidClassBuilder},
        page::{Consumer, Keyboard},
        UsbHidError,
    };
    use ws2812_pio::Ws2812Direct; //old lights code
    //use ws2812_pio::Ws2812;

    type UsbCompositeInterfaceList = HCons<
        ConsumerControlInterface<'static, UsbBus>,
        HCons<NKROBootKeyboardInterface<'static, UsbBus>, HNil>,
    >;
    type UsbCompositeClass<'a> = UsbHidClass<hal::usb::UsbBus, UsbCompositeInterfaceList>;
    type UsbDevice = usb_device::device::UsbDevice<'static, hal::usb::UsbBus>;
    static mut USB_BUS: Option<UsbBusAllocator<hal::usb::UsbBus>> = None;

    // Test PID from https://pid.codes/1209/
    const VID: u16 = 0x1209;
    const PID: u16 = 0x0001;

    type UartPins = (
        hal::gpio::Pin<hal::gpio::pin::bank0::Gpio12, hal::gpio::Function<hal::gpio::Uart>>,
        hal::gpio::Pin<hal::gpio::pin::bank0::Gpio13, hal::gpio::Function<hal::gpio::Uart>>,
    );
    type UartDevice = hal::uart::UartPeripheral<hal::uart::Enabled, hal::pac::UART0, UartPins>;

    type StatusLed = Ws2812Direct<hal::pac::PIO0, hal::pio::SM0, hal::gpio::pin::bank0::Gpio20>;
    enum StatusVal {
        Layer(usize),
        Bootloader,
    }

    pub struct KeyboardState {
        matrix: Matrix<DynPin, DynPin, KBDSIZE_COLS, KBDSIZE_ROWS>,
        debouncer: Debouncer<[[bool; KBDSIZE_COLS]; KBDSIZE_ROWS]>,
    }

    impl KeyboardState {
        pub fn new(
            mut rows: [DynPin; KBDSIZE_ROWS],
            mut cols: [DynPin; KBDSIZE_COLS],
        ) -> KeyboardState {
            for r in rows.iter_mut() {
                r.into_push_pull_output();
                r.set_high().unwrap();
            }
            for c in cols.iter_mut() {
                c.into_pull_up_input();
            }

            KeyboardState {
                matrix: Matrix::new(cols, rows).unwrap(),
                debouncer: Debouncer::new(
                    [[false; KBDSIZE_COLS]; KBDSIZE_ROWS],
                    [[false; KBDSIZE_COLS]; KBDSIZE_ROWS],
                    5,
                ),
            }
        }
    }

    #[monotonic(binds = TIMER_IRQ_0, default = true)]
    type Mono = Rp2040Monotonic;
    type Duration = <Rp2040Monotonic as rtic::Monotonic>::Duration;

    const KBD_SCAN_PERIOD: Duration = Duration::millis(1);
    const USB_KBD_TICK_PERIOD: Duration = Duration::millis(1);
    
    #[shared]
    struct Shared {
        layout: Layout,
        usb_dev: UsbDevice,
        usb_class: UsbCompositeClass<'static>,
        uart: UartDevice,
        rxbuf: [u8; 4],
        //touchpad: Option<Touchpad>,
    }
    

    #[local]
    struct Local {
        kbd_state: KeyboardState,
        is_left: bool,
        status_led: StatusLed,
        transform: fn(Event) -> Event,
        delay: cortex_m::delay::Delay,
    }

    #[init(local = [ layers: Option<Layers> = None])]
    fn init(c: init::Context) -> (Shared, Local, init::Monotonics) {
        // Soft-reset does not release the hardware spinlocks
        // Release them now to avoid a deadlock after debug or watchdog reset
        unsafe {
            hal::sio::spinlock_reset();
        }

        let mut resets = c.device.RESETS;
        let mut watchdog = hal::Watchdog::new(c.device.WATCHDOG);
        let clocks = hal::clocks::init_clocks_and_plls(
            rp_pico::XOSC_CRYSTAL_FREQ,
            c.device.XOSC,
            c.device.CLOCKS,
            c.device.PLL_SYS,
            c.device.PLL_USB,
            &mut resets,
            &mut watchdog,
        )
        .ok()
        .unwrap();

        let timer_mono = Rp2040Monotonic::new(c.device.TIMER);

        let sio = hal::Sio::new(c.device.SIO);
        let pins = rp_pico::Pins::new(
            c.device.IO_BANK0,
            c.device.PADS_BANK0,
            sio.gpio_bank0,
            &mut resets,
        );

        let uart_pins = (
            // UART TX (characters sent from RP2040) on pin 12 (GPIO12)
            pins.gpio12.into_mode::<hal::gpio::FunctionUart>(),
            // UART RX (characters received by RP2040) on pin 13 (GPIO13)
            pins.gpio13.into_mode::<hal::gpio::FunctionUart>(),
        );

        // Make a UART on the given pins
        let mut uart = hal::uart::UartPeripheral::new(c.device.UART0, uart_pins, &mut resets)
            .enable(
                hal::uart::UartConfig::new(
                    38400.Hz(),
                    hal::uart::DataBits::Eight,
                    None,
                    hal::uart::StopBits::One,
                ),
                clocks.peripheral_clock.freq(),
            )
            .unwrap();
        uart.enable_rx_interrupt();

        let kbd_side_pin = pins.gpio0.into_pull_up_input();
        let is_left = kbd_side_pin.is_low().unwrap();
        let transform: fn(Event) -> Event = if is_left {
            |e| e.transform(|i, j| (i, KBDSIZE_COLS as u8 - (j + 1)))
        } else {
            |e| e.transform(|i, j| (i, j + KBDSIZE_COLS as u8))
        };

        let rows: [DynPin; KBDSIZE_ROWS] = [
            pins.gpio1.into(),
            pins.gpio2.into(),
            pins.gpio3.into(),
            pins.gpio4.into(),
            pins.gpio5.into(),
            pins.gpio6.into(),
        ];
        let cols: [DynPin; KBDSIZE_COLS] = [
            pins.gpio7.into(),
            pins.gpio8.into(),
            pins.gpio9.into(),
            pins.gpio10.into(),
            pins.gpio19.into(),
        ];
        let kbd_state = KeyboardState::new(rows, cols);

        let delay =
            cortex_m::delay::Delay::new(c.core.SYST, clocks.system_clock.freq().to_Hz());

        let (mut pio, sm0, _, _, _) = c.device.PIO0.split(&mut resets);
        let mut status_led = Ws2812Direct::new(
            pins.gpio20.into_mode(),
            &mut pio,
            sm0,
            clocks.peripheral_clock.freq(),
        );
        update_status_led(&mut status_led, StatusVal::Layer(0));

        // Set up the USB driver
        let usb_bus = UsbBusAllocator::new(hal::usb::UsbBus::new(
            c.device.USBCTRL_REGS,
            c.device.USBCTRL_DPRAM,
            clocks.usb_clock,
            true,
            &mut resets,
        ));
        let usb_bus = unsafe {
            // Note (safety): This is safe as interrupts haven't been started yet
            USB_BUS = Some(usb_bus);
            USB_BUS.as_ref().unwrap()
        };

        let usb_class = UsbHidClassBuilder::new()
            .add_interface(NKROBootKeyboardInterface::default_config())
            .add_interface(ConsumerControlInterface::default_config())
            //touchpad thing i think
            //.add_interface(WheelMouseInterface::default_config())
            .build(&usb_bus);
        let usb_dev = UsbDeviceBuilder::new(usb_bus, UsbVidPid(VID, PID))
        //FIXED THESE
            .manufacturer("Blue")
            .product("DreamBoard")
            .serial_number(env!("CARGO_PKG_VERSION"))
            .supports_remote_wakeup(true)
            .build();

        info!("Ready!");

        kbd_scan::spawn_after(KBD_SCAN_PERIOD).unwrap();
        usb_keyboard_tick::spawn_after(USB_KBD_TICK_PERIOD).unwrap();

        *c.local.layers = Some(layout::make_keymap(crate::LAYOUT).unwrap());
        let layout = Layout::new(c.local.layers.as_ref().unwrap());
        let shared = Shared {
            layout: layout,
            usb_dev,
            usb_class,
            uart,
            rxbuf: [0; 4],
            //touchpad,
        };
        let local = Local {
            kbd_state,
            is_left,
            status_led,
            transform,
            delay,
        };
        (shared, local, init::Monotonics(timer_mono))
    }

    #[task(binds = USBCTRL_IRQ, shared = [usb_dev, usb_class])]
    fn usbctrl(c: usbctrl::Context) {
        let usb_dev = c.shared.usb_dev;
        let usb_class = c.shared.usb_class;
        (usb_dev, usb_class).lock(|usb_dev, usb_class| {
            if usb_dev.poll(&mut [usb_class]) {
                // need to read any incoming report to clear the interface,
                // even though we don't do anything with it
                let interface = usb_class.interface::<NKROBootKeyboardInterface<'_, _>, _>();
                match interface.read_report() {
                    Err(UsbError::WouldBlock) => {}
                    Err(e) => {
                        core::panic!("Failed to read keyboard report: {:?}", e)
                    }
                    Ok(_) => {}
                }
            }
        });
    }

    #[task(local = [kbd_state, transform, delay], shared = [uart])]
    fn kbd_scan(c: kbd_scan::Context) {
        let transform = c.local.transform;
        let kbd_state = c.local.kbd_state;
        let delay = c.local.delay;
        let matrix_state = kbd_state
            .matrix
            .get_with_delay(|| {
                delay.delay_us(10);
            })
            .unwrap();
        let events = kbd_state.debouncer.events(matrix_state);
        let mut uart = c.shared.uart;
        uart.lock(|uart| {
            for event in events {
                let event = transform(event);
                for &b in &ser(event) {
                    block!(uart.write(b)).unwrap();
                }
                handle_event::spawn(event).unwrap();
            }
        });
        tick_keyboard::spawn().unwrap();
        kbd_scan::spawn_after(KBD_SCAN_PERIOD).unwrap();
    }

    #[task(binds = UART0_IRQ, shared = [uart, rxbuf])]
    fn uart_rx(c: uart_rx::Context) {
        (c.shared.uart, c.shared.rxbuf).lock(|uart, rxbuf| {
            while let Ok(b) = uart.read() {
                rxbuf.rotate_left(1);
                rxbuf[3] = b;

                if rxbuf[3] == b'\n' {
                    if let Ok(event) = de(&rxbuf[..]) {
                        handle_event::spawn(event).unwrap();
                    }
                }
            }
        });
    }

    #[task(capacity = 8, shared = [layout])]
    fn handle_event(mut c: handle_event::Context, event: Event) {
        c.shared.layout.lock(|layout| layout.event(event));
    }

    #[task(shared = [layout],
           local = [is_left, status_led, cur_layer: usize = 0, rset_left: bool = false, rset_count: u32 = 0])]
    fn tick_keyboard(c: tick_keyboard::Context) {
        let mut layout = c.shared.layout;
        let is_left = *c.local.is_left;
        let status_led = c.local.status_led;
        let cur_layer = c.local.cur_layer;
        let rset_left = c.local.rset_left;
        let rset_count = c.local.rset_count;
        layout.lock(|layout| {
            match layout.tick() {
                // reset if reset key pressed 5 times
                CustomEvent::Release(CustomKey::Reset(k)) => {
                    if *rset_count == 0 || k.is_left() != *rset_left {
                        *rset_left = k.is_left();
                        *rset_count = 1;
                    } else if k.is_left() == *rset_left {
                        *rset_count += 1;
                    } else {
                        *rset_count = 0;
                    }
                    if *rset_count >= 5 {
                        *rset_count = 0;
                        update_status_led(status_led, StatusVal::Bootloader);
                        if *rset_left == is_left {
                            do_reset();
                        }
                    } else {
                        update_status_led(status_led, StatusVal::Layer(*cur_layer));
                    }
                }
                CustomEvent::Press(CustomKey::Media(k)) => {
                    let r = MultipleConsumerReport {
                        codes: [
                            *k,
                            Consumer::Unassigned,
                            Consumer::Unassigned,
                            Consumer::Unassigned,
                        ],
                    };
                    send_consumer_report::spawn(r).unwrap();
                }
                CustomEvent::Release(CustomKey::Media(_)) => {
                    send_consumer_report::spawn(MultipleConsumerReport::default()).unwrap();
                }
                _ => {}
            };

            if layout.current_layer() != *cur_layer {
                *cur_layer = layout.current_layer();
                if *cur_layer != 3 {
                    *rset_count = 0;
                }
                update_status_led(status_led, StatusVal::Layer(*cur_layer));
            }

            let keycodes: heapless::Vec<Keyboard, 70> = layout.keycodes().collect();
            send_keyboard_report::spawn(keycodes).unwrap();
        });
    }

    #[task(shared = [usb_class])]
    fn usb_keyboard_tick(c: usb_keyboard_tick::Context) {
        let mut usb_class = c.shared.usb_class;
        usb_class.lock(|usb_class| {
            match usb_class
                .interface::<NKROBootKeyboardInterface<'_, _>, _>()
                .tick()
            {
                Err(UsbHidError::WouldBlock) | Err(UsbHidError::Duplicate) | Ok(_) => {}
                Err(e) => {
                    core::panic!("Failed to process keyboard tick: {:?}", e);
                }
            }
        });
        usb_keyboard_tick::spawn_after(USB_KBD_TICK_PERIOD).unwrap();
    }

    #[task(shared = [usb_dev, usb_class], capacity = 8)]
    fn send_keyboard_report(c: send_keyboard_report::Context, report: heapless::Vec<Keyboard, 70>) {
        let usb_dev = c.shared.usb_dev;
        let usb_class = c.shared.usb_class;
        (usb_dev, usb_class).lock(|usb_dev, usb_class| {
            if usb_dev.state() != usb_device::device::UsbDeviceState::Configured {
                return;
            }
            let is_any_key_pressed = !report.is_empty();
            match usb_class
                .interface::<NKROBootKeyboardInterface<'_, _>, _>()
                .write_report(report)
            {
                Err(UsbHidError::WouldBlock) | Err(UsbHidError::Duplicate) | Ok(_) => {}
                Err(e) => {
                    core::panic!("Failed to write keyboard report: {:?}", e);
                }
            }

            if is_any_key_pressed
                && usb_dev.state() == UsbDeviceState::Suspend
                && usb_dev.remote_wakeup_enabled()
            {
                usb_dev.bus().remote_wakeup();
            }
        });
    }

    #[task(shared = [usb_class], capacity = 8)]
    fn send_consumer_report(c: send_consumer_report::Context, report: MultipleConsumerReport) {
        let mut usb_class = c.shared.usb_class;
        usb_class.lock(|usb_class| {
            match usb_class
                .interface::<ConsumerControlInterface<'_, _>, _>()
                .write_report(&report)
            {
                Err(UsbError::WouldBlock) | Ok(_) => {}
                Err(e) => {
                    core::panic!("Failed to write consumer report: {:?}", e);
                }
            }
        });
    }

    fn de(bytes: &[u8]) -> Result<Event, ()> {
        match *bytes {
            [b'P', i, j, b'\n'] => Ok(Event::Press(i, j)),
            [b'R', i, j, b'\n'] => Ok(Event::Release(i, j)),
            _ => Err(()),
        }
    }

    fn ser(e: Event) -> [u8; 4] {
        match e {
            Event::Press(i, j) => [b'P', i, j, b'\n'],
            Event::Release(i, j) => [b'R', i, j, b'\n'],
        }
    }

    fn update_status_led(status_led: &mut StatusLed, status: StatusVal) {
        let led_color: RGB8 = match status {
            StatusVal::Layer(1) => (0, 0, 40),
            StatusVal::Layer(2) => (8, 0, 32),
            StatusVal::Bootloader => (8, 4, 0),
            _ => (0, 0, 0),
        }
        .into();
        let led_array = [led_color; 7];
        status_led.write(led_array.iter().copied()).unwrap();
    }

    fn do_reset() {
        hal::rom_data::reset_to_usb_boot(0, 0);
    }
}
