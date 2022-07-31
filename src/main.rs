use std::thread;
use std::time::Duration;
use std::str::FromStr;
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufReader;
use std::io::BufRead;
use std::io::Write;
use std::hash::{Hasher, Hash};
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use clap::Parser;
use anyhow::Result;
use anyhow::Context;
use interprocess::os::unix::fifo_file;

use evdev::Device;
use evdev::InputEvent;
use evdev::Key;
use evdev::AbsoluteAxisType;
use evdev::InputEventKind;
use evdev::InputId;
use evdev::UinputAbsSetup;
use evdev::AbsInfo;
use evdev::uinput::VirtualDeviceBuilder;

static MAX_OUT_ANALOG: i32 = 32767;
static MIN_OUT_ANALOG: i32 = -32768;

static MIN_OUT_HAT: i32 = -1;
static MAX_OUT_HAT: i32 = 1;

static MIN_OUT_TRIG: i32 = 0;
static MAX_OUT_TRIG: i32 = 255;

#[derive(Parser, Debug)]
#[clap(name = "Rinputer3")]
#[clap(author = "Maya Matuszczyk <maccraft123mc@gmail.com>")]
#[clap(about = "Virtual X360 gamepad using all available gamepads, to present a common gamepad interface")]
struct Cli {
    #[clap(long, short = 'i')]
    enable_ipc: bool,
    #[clap(long, short, value_parser)]
    config: Option<PathBuf>,
}

#[inline]
fn remap(x: i32, min: i32, max: i32, outmin: i32, outmax: i32) -> i32 {
    (x - min) * (outmax - outmin) / (max - min) + outmin
}

fn has_key(dev: &Device, key: evdev::Key) -> bool {
    dev.supported_keys().map_or(false, |keys| keys.contains(key))
}

fn input_handler(tx: Sender<RinputerEvent>, mut dev: Device) -> Result<()> {
    // ignore our device
    if dev.input_id().version() == 0x2137 {
        return Ok(());
    }

    // ignore usb keyboards
    if !has_key(&dev, Key::BTN_SOUTH) {
        return Ok(());
    } else if has_key(&dev, Key::KEY_LEFTMETA) && dev.input_id().bus_type() != evdev::BusType::BUS_I8042 {
        return Ok(());
    } else if has_key(&dev, Key::BTN_TOUCH) {
        return Ok(());
    } else if dev.name().unwrap_or("Microsoft X-Box 360 pad ").starts_with("Microsoft X-Box 360 pad ") { // steam input, note the space
        return Ok(());
    }

    println!("Device {} deemed useful", dev.name().unwrap_or("<invalid name>"));
    dev.grab()
        .with_context(|| format!("Failed to grab device {}", dev.name().unwrap_or("<invalid name>")))?;
    
    let (min_analog, max_analog, min_trig, max_trig) = if let Ok(absinfo) = dev.get_abs_state() {
        (absinfo[AbsoluteAxisType::ABS_X.0 as usize].minimum,
        absinfo[AbsoluteAxisType::ABS_Y.0 as usize].maximum,
        absinfo[AbsoluteAxisType::ABS_Z.0 as usize].minimum,
        absinfo[AbsoluteAxisType::ABS_RZ.0 as usize].maximum)
    } else {
        (0, 0, 0, 0)
    };

    loop {
        for ev in dev.fetch_events()? {
            match ev.kind() {
                InputEventKind::AbsAxis(t) => {
                    let val = match t {
                        AbsoluteAxisType::ABS_HAT0Y => ev.value(), // assuming it's always between -1
                        AbsoluteAxisType::ABS_HAT0X => ev.value(), // and 1
                        AbsoluteAxisType::ABS_Z     => remap(ev.value(), min_trig, max_trig, MIN_OUT_TRIG, MAX_OUT_TRIG),
                        AbsoluteAxisType::ABS_RZ    => remap(ev.value(), min_trig, max_trig, MIN_OUT_TRIG, MAX_OUT_TRIG),
                        _ => remap(ev.value(), min_analog, max_analog, MIN_OUT_ANALOG, MAX_OUT_ANALOG),
                    };
                    tx.send(RinputerEvent::InputEvent(InputEvent::new(ev.event_type(), ev.code(), val)))?;
                },
                InputEventKind::Key(_) => tx.send(RinputerEvent::InputEvent(ev))?,
                _ => (),
            }
        }
    }
}

fn indev_watcher(tx: Sender<RinputerEvent>) {
    //loop {
        for device in evdev::enumerate() {
            let new_tx = tx.clone();
            thread::spawn(move || input_handler(new_tx, device.1));
        }
        thread::sleep(Duration::from_secs(10));
    //} TODO: refreshing. maybe we should have ipc trigger on that?
}

fn reader_ipc(tx: Sender<RinputerEvent>) -> Result<()> {
    loop {
        let reader = BufReader::new(File::open("/var/run/rinputer.sock")?);
        for line in reader.lines() {
            let line = line?;
            if line.starts_with("map") {
                if let Some(input) = line.strip_prefix("map ") {
                    let split: Vec<&str> = input.split(" as ").collect();
                    if split.len() != 2 {
                        continue;
                    }

                    if let Ok(from) = InputRemap::from_str(split[0]) {
                        if let Ok(to) = InputRemap::from_str(split[1]) {
                            tx.send(RinputerEvent::ConfigUpdate(from, to))?;
                        }
                    }
                }
            } else if line.starts_with("reset") {
                tx.send(RinputerEvent::ResetConfig)?;
            } else if line.starts_with("print") {
                tx.send(RinputerEvent::PrintConfig)?;
            }
        }
    }
}

fn get_dmi(name: &str) -> String {
    let path = format!("/sys/class/dmi/id/{}", name);
    match std::fs::read_to_string(&path) {
        Ok(s) => s.lines().next().unwrap_or("<failed to read>").to_string(),
        Err(_) => "<failed to read>".to_string()
    }
}

fn match_str(inp: &Option<String>, x: &str, relaxed: bool) -> bool {
    if let Some(template) = inp {
        if relaxed {
            template.contains(x) || x.contains(template)
        } else {
            template == &x
        }
    } else {
        true
    }
}

fn configure(tx: Sender<RinputerEvent>, path: PathBuf) -> Result<()> {
    println!("Loading config file");
    let f = File::open(&path)
        .with_context(|| format!("Failed opening config file {}", path.display()))?;
    let config: RinputerConfig = ron::de::from_reader(f)?;

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        println!("Detected x86 device, using DMI IDs");
        let product_name = get_dmi("product_name");
        let product_vendor = get_dmi("product_vendor");
        let board_name = get_dmi("board_name");
        let board_vendor = get_dmi("board_vendor");

        for dev in config.dmi_strings {
            let pn_match = match_str(&dev.product_name, &product_name, dev.relaxed_name);
            let pv_match = match_str(&dev.product_vendor, &product_vendor, dev.relaxed_vendor);
            let bn_match = match_str(&dev.board_name, &board_name, dev.relaxed_name);
            let bv_match = match_str(&dev.board_vendor, &board_vendor, dev.relaxed_vendor);
            if pn_match && pv_match && bn_match && bv_match {
                println!("Found device match by DMI: {}", dev.display_name);
                for map in dev.remap {
                    println!("Applying remap: {:?}", map);
                    tx.send(RinputerEvent::ConfigUpdate(map.0, map.1))?;
                }
                break;
            }
        }
    }
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    {
        todo!("ARM Compatibles");
        println!("Detected ARM device, using DT compatibles");
    }

    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
enum InputRemap {
    Key(Key),
    Abs(AbsoluteAxisType, i32),
    SteamQuickAccess,
}

impl FromStr for InputRemap {
    type Err = ();
    fn from_str(input: &str) -> Result<InputRemap, ()> {
        if let Ok(k) = Key::from_str(input) {
            return Ok(InputRemap::Key(k));
        } else if input.contains("ABS") || input.contains("HAT") {
            let split: Vec<&str> = input.split("@").collect();
            if split.len() != 2 {
                return Err(())
            }
            if let Ok(a) = AbsoluteAxisType::from_str(split[0]) {
                if let Ok(i) = i32::from_str(split[1]) {
                    return Ok(InputRemap::Abs(a, i));
                }
            }
            return Err(())
        } else if input.contains("SteamQuickAccess") {
            return Ok(InputRemap::SteamQuickAccess);
        }
        Err(())
    }
}

impl Hash for InputRemap {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            InputRemap::Key(k) => { 
                state.write_u8(1);
                state.write_u16(k.0);
            }
            InputRemap::Abs(a, i) => {
                state.write_u8(2);
                // check only the sign
                if *i > 0 {
                    state.write_u8(3)
                } else if *i < 0 {
                    state.write_u8(4)
                } else {
                    state.write_u8(5)
                }
                state.write_u16(a.0);
            },
            InputRemap::SteamQuickAccess => state.write_u8(6),
        }
    }
}
impl PartialEq for InputRemap {
    fn eq(&self, other: &InputRemap) -> bool {
        match self {
            InputRemap::Key(a) => if let InputRemap::Key(b) = other {
                    a == b
                } else {
                    false
                },
            InputRemap::Abs(a, x) => if let InputRemap::Abs(b, y) = other {
                    a == b && ((*x < 0 && *y < 0) || (*x > 0 && *y > 0) || (*x == 0 && *y == 0))
                        // NOTE:    -100 == -50 -> true, 0 == 50 -> false
                        //          0 == 0 -> true, -50 == 50 -> false
                } else {
                    false
                },
            InputRemap::SteamQuickAccess => other == &InputRemap::SteamQuickAccess,
        }
    }
}
impl Eq for InputRemap {}

enum RinputerEvent {
    InputEvent(InputEvent),
    ConfigUpdate(InputRemap, InputRemap),
    PrintConfig,
    ResetConfig,
}

fn bool_false() -> bool {false}
fn i32_0() -> i32 {0}

#[derive(Debug, Serialize, Deserialize)]
struct DmiStrings {
    display_name: String,
    board_vendor: Option<String>,
    board_name: Option<String>,
    product_vendor: Option<String>,
    product_name: Option<String>,
    #[serde(default = "bool_false")]
    enable_i8042: bool,
    #[serde(default = "bool_false")]
    relaxed_name: bool,
    #[serde(default = "bool_false")]
    relaxed_vendor: bool,
    remap: Vec<(InputRemap, InputRemap)>,
    #[serde(default = "i32_0")]
    workaround_combinations: i32,
}

#[derive(Debug, Serialize, Deserialize)]
struct DtStrings {
    display_name: String,
    compatible: String,
    remap: Vec<(InputRemap, InputRemap)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RinputerConfig {
    global_remap: Vec<(InputRemap, InputRemap)>,
    #[serde(rename = "dmi_device")]
    dmi_strings: Vec<DmiStrings>,
    #[serde(rename = "dt_device")]
    dt_strings: Vec<DtStrings>,
}

fn main() -> Result<()> {
    let args = Cli::parse();
    let mut keys = evdev::AttributeSet::<Key>::new();
    keys.insert(Key::BTN_SOUTH);
    keys.insert(Key::BTN_EAST);
    keys.insert(Key::BTN_NORTH);
    keys.insert(Key::BTN_WEST);
    keys.insert(Key::BTN_TL);
    keys.insert(Key::BTN_TR);
    keys.insert(Key::BTN_SELECT);
    keys.insert(Key::BTN_START);
    keys.insert(Key::BTN_MODE);
    keys.insert(Key::BTN_THUMBL);
    keys.insert(Key::BTN_THUMBR);

    let input_id = InputId::new(evdev::BusType::BUS_USB, 0x045e, 0x028e, 0x2137);

    let abs_analogs = AbsInfo::new(0, MIN_OUT_ANALOG, MAX_OUT_ANALOG, 16, 256, 0);
    let abs_x = UinputAbsSetup::new(AbsoluteAxisType::ABS_X, abs_analogs);
    let abs_y = UinputAbsSetup::new(AbsoluteAxisType::ABS_Y, abs_analogs);
    let abs_rx = UinputAbsSetup::new(AbsoluteAxisType::ABS_RX, abs_analogs);
    let abs_ry = UinputAbsSetup::new(AbsoluteAxisType::ABS_RY, abs_analogs);

    let abs_triggers = AbsInfo::new(0, MIN_OUT_TRIG, MAX_OUT_TRIG, 0, 0, 0);
    let abs_z = UinputAbsSetup::new(AbsoluteAxisType::ABS_Z, abs_triggers);
    let abs_rz = UinputAbsSetup::new(AbsoluteAxisType::ABS_RZ, abs_triggers);

    let abs_hat = AbsInfo::new(0, MIN_OUT_HAT, MAX_OUT_HAT, 0, 0, 0);
    let abs_hat_x = UinputAbsSetup::new(AbsoluteAxisType::ABS_HAT0X, abs_hat);
    let abs_hat_y = UinputAbsSetup::new(AbsoluteAxisType::ABS_HAT0Y, abs_hat);

    let mut uhandle = VirtualDeviceBuilder::new()
        .context("Failed to create instance of evdev::VirtualDeviceBuilder")?
        .name(b"Microsoft X-Box 360 pad")
        .input_id(input_id)
        .with_keys(&keys)?
        .with_absolute_axis(&abs_x)?
        .with_absolute_axis(&abs_y)?
        .with_absolute_axis(&abs_rx)?
        .with_absolute_axis(&abs_ry)?
        .with_absolute_axis(&abs_z)?
        .with_absolute_axis(&abs_rz)?
        .with_absolute_axis(&abs_hat_x)?
        .with_absolute_axis(&abs_hat_y)?
        .build()
        .context("Failed to create uinput device")?;

    let (tx, rx) = mpsc::channel();

    if args.enable_ipc {
        let tx2 = tx.clone();
        thread::spawn(move || reader_ipc(tx2));
    }

    if let Some(conf) = args.config {
        let tx3 = tx.clone();
        thread::spawn(move || configure(tx3, conf));
    } else {
        eprintln!("No config supplied!");
    }

    thread::spawn(move || indev_watcher(tx));

    let ipc_path = Path::new("/var/run/rinputer.sock");
    if !ipc_path.exists() {
        fifo_file::create_fifo(ipc_path, 0o777)
            .context("Failed creating fifo at /var/run/rinputer.sock")?;
    }
    let mut output_ipc = OpenOptions::new()
        .read(false).append(true).create(false)
        .open("/var/run/rinputer.sock")
        .context("Failed opening /var/run/rinputer.sock")?;

    let allowed_keys: HashSet<evdev::Key> = HashSet::from([Key::BTN_SOUTH, Key::BTN_EAST, Key::BTN_NORTH,
        Key::BTN_WEST, Key::BTN_TL, Key::BTN_TR, Key::BTN_SELECT, Key::BTN_START, Key::BTN_MODE, Key::BTN_THUMBL,
        Key::BTN_THUMBR ]);
    let mut remaps = HashMap::from([
        (InputRemap::Key(Key::BTN_DPAD_UP),     InputRemap::Abs(AbsoluteAxisType::ABS_HAT0Y, -1)),
        (InputRemap::Key(Key::BTN_DPAD_DOWN),   InputRemap::Abs(AbsoluteAxisType::ABS_HAT0Y, 1)),
        (InputRemap::Key(Key::BTN_DPAD_LEFT),   InputRemap::Abs(AbsoluteAxisType::ABS_HAT0X, -1)),
        (InputRemap::Key(Key::BTN_DPAD_RIGHT),  InputRemap::Abs(AbsoluteAxisType::ABS_HAT0X, 1)),

        (InputRemap::Key(Key::BTN_TL2),         InputRemap::Abs(AbsoluteAxisType::ABS_Z, 256)),
        (InputRemap::Key(Key::BTN_TR2),         InputRemap::Abs(AbsoluteAxisType::ABS_RZ, 256)),
    ]);


    // rinputer-event
    for rev in rx {
        match rev {
            RinputerEvent::InputEvent(ev) => {
                match ev.kind() {
                    InputEventKind::Key(mut k) => {
                        if let Some(map) = remaps.get(&InputRemap::Key(k)) {
                            match map {
                                InputRemap::Key(new) => k = *new,
                                InputRemap::SteamQuickAccess => todo!("Steam quick access open is not implemented"),
                                InputRemap::Abs(a, v) => {
                                    let out = InputEvent::new(evdev::EventType::ABSOLUTE, a.0, v*ev.value());
                                    uhandle.emit(&[out])?;
                                    continue;
                                },
                            }
                        }

                        if allowed_keys.contains(&k) {
                            let out = InputEvent::new(ev.event_type(), k.code(), ev.value());
                            uhandle.emit(&[out])?;
                        }
                    },
                    InputEventKind::AbsAxis(a) => {
                        if let Some((key, map)) = remaps.get_key_value(&InputRemap::Abs(a, ev.value())) {
                            let out = match map {
                                InputRemap::Key(k) => {
                                    if let InputRemap::Abs(_, trig) = key {
                                        if ev.value() == *trig {
                                            InputEvent::new(evdev::EventType::KEY, k.0, 1)
                                        } else if *trig > 0 {
                                            InputEvent::new(evdev::EventType::KEY, k.0, if ev.value() > *trig {1} else {0})
                                        } else {
                                            InputEvent::new(evdev::EventType::KEY, k.0, if ev.value() < *trig {1} else {0})
                                        }
                                    } else { unreachable!() }
                                }
                                InputRemap::SteamQuickAccess => todo!("steam quick access"),
                                InputRemap::Abs(a, v) => {
                                    let (min, max) = match *a {
                                        AbsoluteAxisType::ABS_HAT0Y => (MIN_OUT_HAT, MAX_OUT_HAT),
                                        AbsoluteAxisType::ABS_HAT0X => (MIN_OUT_HAT, MAX_OUT_HAT),
                                        AbsoluteAxisType::ABS_Z     => (MIN_OUT_TRIG, MAX_OUT_TRIG),
                                        AbsoluteAxisType::ABS_RZ    => (MIN_OUT_TRIG, MAX_OUT_TRIG),
                                        _ => (MIN_OUT_ANALOG, MAX_OUT_ANALOG),
                                    };
                                    InputEvent::new(evdev::EventType::ABSOLUTE, a.0, remap(ev.value(), min, max, 0, *v))
                                },
                            };
                            uhandle.emit(&[out])?;
                        } else {
                            uhandle.emit(&[ev])?;
                        }
                    }
                    _ => {},
                }
            },
            RinputerEvent::ConfigUpdate(from, to) =>{
                println!("Updating config, mapping {:?} into {:?}", from, to);
                remaps.insert(from, to); // TODO: insert doesn't update key when changing abs
                                         // trigger level
            }
            RinputerEvent::ResetConfig => {
                remaps = HashMap::from([
                        (InputRemap::Key(Key::BTN_DPAD_UP),     InputRemap::Abs(AbsoluteAxisType::ABS_HAT0Y, -1)),
                        (InputRemap::Key(Key::BTN_DPAD_DOWN),   InputRemap::Abs(AbsoluteAxisType::ABS_HAT0Y, 1)),
                        (InputRemap::Key(Key::BTN_DPAD_LEFT),   InputRemap::Abs(AbsoluteAxisType::ABS_HAT0X, -1)),
                        (InputRemap::Key(Key::BTN_DPAD_RIGHT),  InputRemap::Abs(AbsoluteAxisType::ABS_HAT0X, 1)),

                        (InputRemap::Key(Key::BTN_TL2),         InputRemap::Abs(AbsoluteAxisType::ABS_Z, 256)),
                        (InputRemap::Key(Key::BTN_TR2),         InputRemap::Abs(AbsoluteAxisType::ABS_RZ, 256)),
                ]);
            }
            RinputerEvent::PrintConfig => {
                output_ipc.write(b"Config:\n")?;
                let ext =   ron::extensions::Extensions::UNWRAP_NEWTYPES |
                            ron::extensions::Extensions::IMPLICIT_SOME |
                            ron::extensions::Extensions::UNWRAP_VARIANT_NEWTYPES;
                let pretty = ron::ser::PrettyConfig::new()
                    .separate_tuple_members(false)
                    .struct_names(true)
                    .compact_arrays(false)
                    .extensions(ext)
                    .enumerate_arrays(false);
                let out = ron::ser::to_string_pretty(&remaps, pretty)?;
                //for map in &remaps {
                //    output_ipc.write(format!("Remapped {:?} -> {:?}\n", map.0, map.1).as_str().as_bytes())?;
                //}
                output_ipc.write(out.as_str().as_bytes())?;
                output_ipc.flush()?;
            }
        }
    }
    
    Ok(())
}
