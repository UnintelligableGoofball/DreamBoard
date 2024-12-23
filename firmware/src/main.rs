#![no_std]
#![no_main]

use panic_probe as _;
use adafruit_kb2040;
use adafruit_kb2040::hal::{
    clocks::{init_clocks_and_plls, Clock},
    gpio::{bank0::*, dynpin::DynPin},
    pac::{I2C0, PIO0},
    pio::{PIOExt, SM0, SM1},
    sio::Sio,
    timer::{Alarm0, Timer},
    usb::UsbBus,
    watchdog::Watchdog,
};
