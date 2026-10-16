use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

use anyhow::Result;
use log::{debug, trace};
use serde::Deserialize;

use crate::battery::Battery;

#[derive(Deserialize)]
struct BatteryCommandOutput {
    battery: i32,
    #[allow(dead_code)]
    voltage: i32,
    charging: i32,
}

pub struct Miyoo354Battery {
    charging: bool,
    percentage: i32,
    sampled: bool,
}

impl Miyoo354Battery {
    pub fn new() -> Miyoo354Battery {
        Miyoo354Battery {
            charging: false,
            percentage: 100,
            sampled: false,
        }
    }
}

impl Battery for Miyoo354Battery {
    fn update(&mut self) -> Result<()> {
        self.sampled = false;
        let mut child = Command::new("/customer/app/axp_test")
            .stdout(Stdio::piped())
            .spawn()?;
        match child.wait_timeout(Duration::from_millis(100))? {
            Some(_) => (),
            None => {
                child.kill()?;
                child.wait()?;
                return Ok(());
            }
        }
        let output: BatteryCommandOutput = serde_json::from_reader(child.stdout.unwrap())?;
        self.percentage = output.battery;
        self.charging = output.charging == 3;
        self.sampled = true;

        trace!("battery: {}%", self.percentage);
        Ok(())
    }

    fn sample(&mut self) -> Result<Option<(i32, bool)>> {
        self.update()?;
        Ok(self.sampled.then_some((self.percentage, self.charging)))
    }

    fn percentage(&self) -> i32 {
        self.percentage
    }

    fn charging(&self) -> bool {
        self.charging
    }

    fn update_led(enabled: bool) {
        debug!("setting battery LED to {}", enabled);
        if !Path::new("/sys/class/gpio/gpio86").exists() {
            let _ = File::create("/sys/class/gpio/export").and_then(|mut f| f.write_all(b"86"));
        }
        let _ = File::create("/sys/class/gpio/gpio86/value")
            .and_then(|mut f| f.write_all(if enabled { b"1" } else { b"0" }));
    }
}
