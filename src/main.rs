use std::{
    env,
    fs::{self, File},
    io::BufReader,
    path::PathBuf,
    thread,
    time::Duration,
};

use cpal::{
    platform::Device,
    traits::{DeviceTrait, HostTrait},
};
use rodio::{Decoder, OutputStreamBuilder, Sink};

const FILE_EXTENSIONS: [&'static str; 6] = [
    "flac", "mp3", "ogg", // vorbis
    "wav", "mp4", "m4a",
];

struct State {
    devices: Vec<Device>,
    sources: Vec<PathBuf>,
    default_device: Option<Device>,
    source_index: usize,
    random_seed: Option<u64>,
    volume: f32,
    speed: f32,
    seek: Option<Duration>,
}

#[derive(Debug)]
enum AppError {
    Io(std::io::Error),
    Stream(rodio::stream::StreamError),
    Decoder(rodio::decoder::DecoderError),
    Seek(rodio::source::SeekError),
}

impl State {
    fn new() -> Self {
        let mut state = Self {
            devices: Vec::with_capacity(4),
            sources: Vec::with_capacity(32),
            default_device: None,
            source_index: 0,
            random_seed: None,
            volume: 0.8,
            speed: 1.0,
            seek: None,
        };
        state.get_devices();
        state
    }

    fn start(&mut self, device: Device) -> Result<(), AppError> {
        let stream_handle = OutputStreamBuilder::from_device(device)
            .map_err(AppError::Stream)?
            .open_stream_or_fallback()
            .map_err(AppError::Stream)?;
        let mixer = stream_handle.mixer();
        let sink = Sink::connect_new(&mixer);

        loop {
            assert!(sink.empty());
            if self.source_index == self.sources.len() {
                break;
            }
            let file = File::open(&self.sources[self.source_index]).map_err(AppError::Io)?;
            let source = Decoder::try_from(BufReader::new(file)).map_err(AppError::Decoder)?;

            sink.append(source);
            self.source_index += 1;

            sink.play();
            println!("playing {:?}", &self.sources[self.source_index]);
            self.loop_poll_position(&sink);
        }

        Ok(())
    }

    fn loop_poll_position(&mut self, sink: &Sink) {
        loop {
            if !sink.empty() {
                let pos = sink.get_pos();
                self.seek = Some(pos);
            } else {
                self.seek = None;
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }

    fn switch_device(&mut self, device: Device) -> Result<(), AppError> {
        // self.source_index points to next song
        self.source_index = self.source_index.saturating_sub(1);

        let stream_handle = OutputStreamBuilder::from_device(device)
            .map_err(AppError::Stream)?
            .open_stream_or_fallback()
            .map_err(AppError::Stream)?;
        let mixer = stream_handle.mixer();

        let file = File::open(&self.sources[self.source_index]).map_err(AppError::Io)?;
        let sink = Sink::connect_new(&mixer);

        let source = Decoder::try_from(BufReader::new(file)).map_err(AppError::Decoder)?;
        sink.append(source);

        // NOTE: no need to carry over previous volume, speed, and location
        // TODO: this is never set, so i probably want to set it during playback
        if let Some(seek) = self.seek {
            sink.try_seek(seek).map_err(AppError::Seek)?;
        }
        self.source_index += 1;
        sink.play();
        println!("playing {:?}", &self.sources[self.source_index]);
        self.loop_poll_position(&sink);

        loop {
            assert!(sink.empty());
            if self.source_index == self.sources.len() {
                break;
            }
            let file = File::open(&self.sources[self.source_index]).map_err(AppError::Io)?;
            let source = Decoder::try_from(BufReader::new(file)).map_err(AppError::Decoder)?;

            sink.append(source);
            self.source_index += 1;

            sink.play();
            println!("playing {:?}", &self.sources[self.source_index]);
            self.loop_poll_position(&sink);
        }

        Ok(())
    }

    // TODO: handle devices being connected at runtime
    fn get_devices(&mut self) {
        // enumerate and store initially detected devices
        let host = cpal::default_host();
        self.default_device = host.default_output_device();

        if let Ok(devices) = host.devices() {
            for device in devices {
                let name = device.name().unwrap_or_default();
                if device.supports_output() {
                    println!("{} supports output", name);
                    if let Ok(configs) = device.supported_output_configs() {
                        for cfg in configs {
                            println!("{:#?}", cfg);
                        }
                    }
                    self.devices.push(device);
                } else {
                    println!("{} doesn't support output", name);
                }
            }
        }
    }
}

struct Args {
    dir: String,
}

impl Args {
    fn parse() -> Self {
        let mut args = env::args();
        let _ = args.next();
        let dir = args.next().expect("usage: audio_player <DIR>");

        Args { dir }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let mut state = State::new();

    if let Ok(mut read_dir) = fs::read_dir(&args.dir) {
        while let Some(Ok(entry)) = read_dir.next() {
            if entry
                .path()
                .extension()
                .is_some_and(|ext| FILE_EXTENSIONS.contains(&ext.to_string_lossy().as_ref()))
            {
                state.sources.push(entry.path());
            }
        }
    }

    if state.sources.is_empty() {
        panic!("there's nothing to play you dingus");
    }
    state.sources.sort();

    // try to use default device first
    // on macOS, the sound control panel allows the user to change the default device
    // the default device can be dynamically changed by the user and doesn't require any rebuilding of
    // the sources and sink in our code
    // for example we can have the default device, monitor, wireless earbuds, externally connected
    // speaker, laptop speaker, etc.
    //
    // if the user explicitly selects a device such as earbuds, it's used
    // until explicitly selecting a different device
    if let Some(default_device) = &state.default_device {
        state.start(default_device.clone()).unwrap();
    } else if !state.devices.is_empty() {
        state.start(state.devices[0].clone()).unwrap();
    } else {
        panic!("no output devices are available");
    }

    Ok(())
}
