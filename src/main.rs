#![no_std]
#![no_main]

mod map_range;
mod motor;

use crate::map_range::map_range;
use crate::motor::{Motor, MotorDirection};
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer};
use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcCalLine, AdcConfig, AdcPin, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::GpioPin;
use esp_hal::ledc::timer::TimerIFace;
use esp_hal::peripherals::ADC1;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::timer::ErasedTimer;
use esp_hal::{
    clock::ClockControl,
    gpio::Io,
    ledc::{
        channel,
        timer::{self},
        LSGlobalClkSource, Ledc, LowSpeed,
    },
    peripherals::Peripherals,
    prelude::*,
    system::SystemControl,
};
use log::{debug, info};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use static_cell::StaticCell;

const NUM_ADC_SAMPLES: usize = 100; // Number of ADC samples to average
const MAX_ACTIVE_SEC: u16 = 10 * 60; // Number of seconds the device will be active before going to deep sleep
const MIN_MOTOR_DUTY_PERCENT: u8 = 20;
const MAX_MOTOR_DUTY_PERCENT: u8 = 100;
const MIN_MOVEMENT_DURATION: u16 = 200; // ms
const MAX_MOVEMENT_DURATION: u16 = 2_000; // ms
const POTENTIOMETER_READ_INTERVAL: u16 = 200; // ms

const MIN_ADC_VOLTAGE: u16 = 0; // mV
const MAX_ADC_VOLTAGE: u16 = 3000; // mV

static CURRENT_MAX_MOTOR_PERCENT: AtomicU8 = AtomicU8::new(MIN_MOTOR_DUTY_PERCENT);
static CURRENT_MAX_MOVEMENT_DURATION: AtomicU16 = AtomicU16::new(MIN_MOVEMENT_DURATION);
static DRASTIC_PARAMETER_CHANGE: AtomicBool = AtomicBool::new(false);

type RtcMutex = Mutex<CriticalSectionRawMutex, Rtc<'static>>;

type Adc1Calibration = AdcCalLine<ADC1>;
type Adc1Mutex = Mutex<CriticalSectionRawMutex, Adc<'static, ADC1>>;
type AdcPin0MutexForSpeed =
    Mutex<CriticalSectionRawMutex, AdcPin<GpioPin<0>, ADC1, Adc1Calibration>>;
type AdcPin1MutexForDuration =
    Mutex<CriticalSectionRawMutex, AdcPin<GpioPin<1>, ADC1, Adc1Calibration>>;

#[main]
async fn main(spawner: Spawner) {
    let peripherals = Peripherals::take();
    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::configure(system.clock_control, CpuClock::Clock80MHz).freeze();
    esp_println::logger::init_logger(log::LevelFilter::Debug);

    let timg0 = TimerGroup::new(peripherals.TIMG0, &clocks);
    let timer0: ErasedTimer = timg0.timer0.into();
    let timg1 = TimerGroup::new(peripherals.TIMG1, &clocks);
    let timer1: ErasedTimer = timg1.timer0.into();

    esp_hal_embassy::init(&clocks, [timer0, timer1]);

    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);
    info!("Hello!");

    static RTC: StaticCell<RtcMutex> = StaticCell::new();
    let rtc = RTC.init(Mutex::new(Rtc::new(peripherals.LPWR)));

    let motor_pwm_pin_forward = io.pins.gpio2;
    let motor_pwm_pin_reverse = io.pins.gpio3;

    // Instantiate PWM infra
    let mut ledc_pwm_controller = Ledc::new(peripherals.LEDC, &clocks);
    ledc_pwm_controller.set_global_slow_clock(LSGlobalClkSource::APBClk);
    let mut pwm_timer = ledc_pwm_controller.get_timer::<LowSpeed>(timer::Number::Timer0);
    pwm_timer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty14Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: 2.kHz(),
        })
        .unwrap();

    let mut motor = Motor::new(
        &ledc_pwm_controller,
        &pwm_timer,
        channel::Number::Channel0,
        channel::Number::Channel1,
        motor_pwm_pin_forward,
        motor_pwm_pin_reverse,
    );

    // Instantiate ADC and mutexes
    let mut adc1_config = AdcConfig::new();
    static SPEED_STATIC_CELL: StaticCell<AdcPin0MutexForSpeed> = StaticCell::new();
    let speed_pot_pin = SPEED_STATIC_CELL.init(Mutex::new(
        adc1_config
            .enable_pin_with_cal::<_, Adc1Calibration>(io.pins.gpio0, Attenuation::Attenuation11dB),
    ));
    static DURATION_STATIC_CELL: StaticCell<AdcPin1MutexForDuration> = StaticCell::new();
    let duration_pot_pin = DURATION_STATIC_CELL.init(Mutex::new(
        adc1_config
            .enable_pin_with_cal::<_, Adc1Calibration>(io.pins.gpio1, Attenuation::Attenuation11dB),
    ));
    let adc1 = Adc::new(peripherals.ADC1, adc1_config);
    static ADC1_MUTEX: StaticCell<Adc1Mutex> = StaticCell::new();
    let adc1 = ADC1_MUTEX.init(Mutex::new(adc1));

    spawner.must_spawn(deep_sleep_countdown(rtc));
    spawner.must_spawn(monitor_speed_pot(adc1, speed_pot_pin));
    spawner.must_spawn(monitor_duration_pot(adc1, duration_pot_pin));

    // Main loop
    let mut small_rng = SmallRng::seed_from_u64(1); // Seed is irrelevant for random number generation
    let mut ticker = Ticker::every(Duration::from_millis(POTENTIOMETER_READ_INTERVAL.into()));
    for direction in [MotorDirection::Forward, MotorDirection::Reverse]
        .iter()
        .cycle()
    {
        let start_time = Instant::now();
        let max_motor_percent = CURRENT_MAX_MOTOR_PERCENT.load(Ordering::Relaxed);
        let duty_percent = small_rng.gen_range(MIN_MOTOR_DUTY_PERCENT..=max_motor_percent);

        let max_movement_duration = CURRENT_MAX_MOVEMENT_DURATION.load(Ordering::Relaxed);
        let movement_duration_ms =
            small_rng.gen_range(MIN_MOVEMENT_DURATION..=max_movement_duration);
        let movement_duration = Duration::from_millis(movement_duration_ms.into());

        motor.start_movement(direction, duty_percent);
        debug!(
            "Movement started: {:?} @ {}% for {} ms",
            direction, duty_percent, movement_duration_ms
        );
        DRASTIC_PARAMETER_CHANGE.store(false, Ordering::Relaxed);

        while Instant::now().duration_since(start_time) <= movement_duration {
            ticker.next().await;
            if DRASTIC_PARAMETER_CHANGE.load(Ordering::Relaxed) {
                debug!("Drastic parameter change detected, breaking loop");
                break; // Break the loop if there is a drastic parameter change
            }
        }
    }
}

#[embassy_executor::task]
async fn deep_sleep_countdown(rtc: &'static RtcMutex) {
    Timer::after(Duration::from_secs(MAX_ACTIVE_SEC.into())).await;
    info!("{} seconds passed, going to deep sleep", MAX_ACTIVE_SEC);
    rtc.lock().await.sleep_deep(&[]);
}

#[embassy_executor::task]
async fn monitor_speed_pot(
    adc1_mutex: &'static Adc1Mutex,
    speed_pot_pin_mutex: &'static AdcPin0MutexForSpeed,
) {
    let mut ticker = Ticker::every(Duration::from_millis(POTENTIOMETER_READ_INTERVAL.into()));
    let mut prev_max_duty_percent = MIN_MOTOR_DUTY_PERCENT;
    loop {
        debug!("Checking speed pot pin (#0)");
        {
            let mut adc1 = adc1_mutex.lock().await;
            let mut speed_pot_pin = speed_pot_pin_mutex.lock().await;
            let avg_pin_value: u16 = ((0..NUM_ADC_SAMPLES)
                .map(|_| nb::block!(adc1.read_oneshot(&mut speed_pot_pin)).unwrap() as u32)
                .sum::<u32>()
                / NUM_ADC_SAMPLES as u32)
                .try_into()
                .expect("Average of ADC readings is too large to fit into u16");
            debug!("Average speed pot pin value: {}", avg_pin_value);
            let max_duty_percent = map_range(
                avg_pin_value as u32,
                MIN_ADC_VOLTAGE.into(),
                MAX_ADC_VOLTAGE.into(),
                MIN_MOTOR_DUTY_PERCENT.into(),
                MAX_MOTOR_DUTY_PERCENT.into(),
            )
            .unwrap()
            .try_into()
            .expect("Max duty percent is too large to fit into u8");
            debug!("Max duty percent: {}", max_duty_percent);
            CURRENT_MAX_MOTOR_PERCENT.store(max_duty_percent, Ordering::Relaxed);

            if prev_max_duty_percent.abs_diff(max_duty_percent) > 10 {
                DRASTIC_PARAMETER_CHANGE.store(true, Ordering::Relaxed);
            }

            prev_max_duty_percent = max_duty_percent;
        }
        ticker.next().await;
    }
}

#[embassy_executor::task]
async fn monitor_duration_pot(
    adc1_mutex: &'static Adc1Mutex,
    duration_pot_pin_mutex: &'static AdcPin1MutexForDuration,
) {
    let mut ticker = Ticker::every(Duration::from_millis(POTENTIOMETER_READ_INTERVAL.into()));
    let mut prev_max_duration = MIN_MOVEMENT_DURATION;
    loop {
        debug!("Checking duration pot pin (#1)");
        {
            let mut adc1 = adc1_mutex.lock().await;
            let mut duration_pot_pin = duration_pot_pin_mutex.lock().await;
            let avg_pin_value: u16 = ((0..NUM_ADC_SAMPLES)
                .map(|_| nb::block!(adc1.read_oneshot(&mut duration_pot_pin)).unwrap() as u32)
                .sum::<u32>()
                / NUM_ADC_SAMPLES as u32)
                .try_into()
                .expect("Average of ADC readings is too large to fit into u16");
            debug!("Average duration pot pin value: {}", avg_pin_value);
            let max_duration = map_range(
                avg_pin_value as u32,
                MIN_ADC_VOLTAGE.into(),
                MAX_ADC_VOLTAGE.into(),
                MIN_MOVEMENT_DURATION.into(),
                MAX_MOVEMENT_DURATION.into(),
            )
            .unwrap()
            .try_into()
            .expect("Max duration is too large to fit into u16");
            debug!("Max duration: {}", max_duration);
            CURRENT_MAX_MOVEMENT_DURATION.store(max_duration, Ordering::Relaxed);

            if prev_max_duration.abs_diff(max_duration) > 10 {
                DRASTIC_PARAMETER_CHANGE.store(true, Ordering::Relaxed);
            }

            prev_max_duration = max_duration;
        }
        ticker.next().await;
    }
}
