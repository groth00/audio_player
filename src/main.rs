use std::{
    collections::HashMap,
    env, fmt,
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::{
    platform::Device,
    traits::{DeviceTrait, HostTrait},
};
use crossterm::event::{self, KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Flex, Layout, Rect},
    prelude::{Color, Stylize, Text},
    style::{palette::tailwind::SLATE, Modifier, Style},
    text::Line,
    widgets::{
        Block, HighlightSpacing, List, ListItem, ListState, Paragraph, StatefulWidget, Widget,
    },
    DefaultTerminal,
};
use rodio::{decoder::DecoderBuilder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use symphonia::{core::codecs::CodecRegistry, default::register_enabled_codecs};
use symphonia_adapter_libopus::OpusDecoder;

const FILE_EXTENSIONS: [&'static str; 5] = ["flac", "mp3", "mp4", "m4a", "opus"];
const LIST_STYLE: Style = Style::new().bg(SLATE.c800).add_modifier(Modifier::BOLD);
const LIST_ITEM_STYLE: Style = Style::new().bg(Color::Cyan);
const DEVICE_ITEM_STYLE: Style = Style::new().bg(Color::LightGreen);

struct PlaybackPosition {
    total_duration: Option<Duration>,
    current_pos: Option<Duration>,
    volume: f32,
    is_looping: bool,
}

impl Default for PlaybackPosition {
    fn default() -> Self {
        Self {
            total_duration: None,
            current_pos: None,
            volume: 0.8,
            is_looping: false,
        }
    }
}

enum Focus {
    Songs,
    Devices,
}

#[derive(Clone)]
struct AvailableDevices {
    output_devices: Vec<Device>,
    current_device: usize,
    names: Vec<String>,
}

impl AvailableDevices {
    fn new(output_devices: Vec<Device>) -> Self {
        let names = AvailableDevices::names(&output_devices);

        Self {
            output_devices,
            current_device: 0,
            names,
        }
    }

    fn is_empty(&self) -> bool {
        self.output_devices.is_empty()
    }

    fn names(output_devices: &[Device]) -> Vec<String> {
        let mut names = Vec::with_capacity(output_devices.len());

        for (i, d) in output_devices.iter().enumerate() {
            if let Ok(desc) = d.description() {
                if i == 0 {
                    names.push("Default Device".to_owned());
                } else {
                    names.push(desc.name().to_owned());
                }
            } else {
                names.push("Unknown".into());
            }
        }
        names
    }
}

struct SongsState {
    playback_jh: JoinHandle<Result<(), AppError>>,
    playback_position: PlaybackPosition,
    main_tx: Sender<Msg2Sub>,
    sub_rx: Receiver<Msg2Main>,
    media: MediaSources,
    devices: AvailableDevices,
    device_list_state: ListState,
    focus: Focus,
    exit_reason: Option<ReasonStopped>,
    message: Option<(Instant, &'static str)>,
}

impl SongsState {
    fn new(
        main_tx: Sender<Msg2Sub>,
        main_rx: Receiver<Msg2Sub>,
        initial_sources: Vec<MediaSource>,
        devices: AvailableDevices,
        initial_status: Status,
    ) -> Self {
        let starting_device = &devices.output_devices[0];

        let (sub_tx, sub_rx): (Sender<Msg2Main>, Receiver<Msg2Main>) = mpsc::channel();
        let playback_jh = match SongsState::start_playback(
            &starting_device,
            &initial_sources,
            main_rx,
            sub_tx,
            initial_status,
        ) {
            Ok(jh) => jh,
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        };

        Self {
            playback_jh,
            playback_position: PlaybackPosition::default(),
            main_tx,
            sub_rx,
            devices,
            device_list_state: ListState::default(),
            media: MediaSources {
                sources: initial_sources,
                state: ListState::default(),
            },
            focus: Focus::Songs,
            exit_reason: None,
            message: None,
        }
    }

    fn start_playback(
        device: &Device,
        sources: &[MediaSource],
        main_rx: Receiver<Msg2Sub>,
        sub_tx: Sender<Msg2Main>,
        status: Status,
    ) -> Result<JoinHandle<Result<(), AppError>>, AppError> {
        let device = device.clone();
        let media_sources = sources.iter().cloned().map(|s| s.path).collect();
        let sub_tx_err = sub_tx.clone();

        let mut stream_handle = DeviceSinkBuilder::from_device(device.clone())
            .map_err(AppError::Sink)?
            .with_error_callback(move |err: cpal::StreamError| match err {
                cpal::StreamError::DeviceNotAvailable => {
                    let _ = sub_tx_err.send(Msg2Main::DeviceLost);
                }
                _ => {
                    let _ = sub_tx_err.send(Msg2Main::PanicS(err.to_string()));
                }
            })
            .open_sink_or_fallback()
            .map_err(AppError::Sink)?;
        stream_handle.log_on_drop(false);

        let mut codec_registry = CodecRegistry::new();
        codec_registry.register_all::<symphonia::default::codecs::AacDecoder>();
        codec_registry.register_all::<symphonia::default::codecs::AlacDecoder>();
        codec_registry.register_all::<symphonia::default::codecs::FlacDecoder>();
        codec_registry.register_all::<symphonia::default::codecs::MpaDecoder>();
        codec_registry.register_all::<OpusDecoder>();
        register_enabled_codecs(&mut codec_registry);

        let codec_registry_arc = Arc::new(codec_registry);

        let jh = thread::spawn(move || -> Result<(), AppError> {
            let stream_handle = stream_handle;
            let mixer = stream_handle.mixer();
            let sink = Player::connect_new(&mixer);
            let codec_registry = codec_registry_arc.clone();

            let mut playback_state = PlaybackState::new(
                sub_tx,
                main_rx,
                stream_handle,
                sink,
                media_sources,
                device,
                status,
                codec_registry,
            );
            playback_state.run()
        });

        Ok(jh)
    }

    fn read_channel(&mut self) {
        for msg in self.sub_rx.try_iter() {
            match msg {
                Msg2Main::LoopStatus(is_looping) => self.playback_position.is_looping = is_looping,
                Msg2Main::Panic(msg) => panic!("{}", msg),
                Msg2Main::PanicS(msg) => panic!("{}", msg),
                Msg2Main::OutputStreamInitializationFailed => {
                    self.message = Some((Instant::now(), "failed to switch devices"));
                }
                Msg2Main::DeviceSwitched(new_device) => {
                    self.devices.current_device = self
                        .devices
                        .output_devices
                        .iter()
                        .position(|d| d.description() == new_device.description())
                        .unwrap_or_default();
                }
                Msg2Main::DeviceLost => {
                    if let Err(_) = self.send_devices(true) {
                        self.exit_reason = Some(ReasonStopped::NoOutputDevices);
                    }
                }
                Msg2Main::DeviceLostWait => {
                    if let Err(_) = self.send_devices(false) {
                        self.exit_reason = Some(ReasonStopped::NoOutputDevices);
                    }
                }
                Msg2Main::SongChanged(index) => match index {
                    Some(song_index) => {
                        self.media
                            .sources
                            .iter_mut()
                            .enumerate()
                            .for_each(|(i, s)| {
                                if i == song_index {
                                    s.is_playing = true;
                                } else {
                                    s.is_playing = false;
                                }
                            });
                    }
                    None => self
                        .media
                        .sources
                        .iter_mut()
                        .for_each(|s| s.is_playing = false),
                },
                Msg2Main::Randomized(sources) => {
                    self.media.sources = sources
                        .into_iter()
                        .enumerate()
                        .map(|(i, s)| MediaSource {
                            path: s,
                            is_playing: if i == 0 { true } else { false },
                        })
                        .collect();
                }
                Msg2Main::TotalDuration(duration) => {
                    self.playback_position.total_duration = duration
                }
                Msg2Main::UpdatePosition(pos) => self.playback_position.current_pos = pos,
                Msg2Main::VolumeChanged(vol) => self.playback_position.volume = vol,
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match self.focus {
            Focus::Songs => match key.code {
                KeyCode::Esc => self.exit_reason = Some(ReasonStopped::Quit),
                KeyCode::Up | KeyCode::Char('k') => self.media.state.select_previous(),
                KeyCode::Down | KeyCode::Char('j') => self.media.state.select_next(),
                KeyCode::Char('G') | KeyCode::Home => self.media.state.select_last(),
                KeyCode::Char('g') | KeyCode::End => self.media.state.select_first(),
                KeyCode::Char('l') => self.focus = Focus::Devices,
                KeyCode::Char('p') | KeyCode::F(8) | KeyCode::Pause | KeyCode::Char(' ') => {
                    let _ = self.main_tx.send(Msg2Sub::PauseUnpause);
                }
                KeyCode::Tab => self.exit_reason = Some(ReasonStopped::ChangeScreen),
                KeyCode::Char('f') => {
                    if let Err(_) = self.send_devices(true) {
                        self.exit_reason = Some(ReasonStopped::NoOutputDevices);
                    }
                }
                KeyCode::Char('r') => {
                    let _ = self.main_tx.send(Msg2Sub::Randomize);
                }
                KeyCode::Char('s') | KeyCode::F(9) => {
                    let _ = self.main_tx.send(Msg2Sub::Skip);
                }
                KeyCode::Char('m') | KeyCode::F(10) => {
                    let _ = self.main_tx.send(Msg2Sub::Mute);
                }
                KeyCode::Char('v') | KeyCode::F(11) => {
                    let _ = self.main_tx.send(Msg2Sub::DecreaseVolume);
                }
                KeyCode::Char('V') | KeyCode::F(12) => {
                    let _ = self.main_tx.send(Msg2Sub::IncreaseVolume);
                }
                KeyCode::Left => {
                    let _ = self.main_tx.send(Msg2Sub::MoveBackward);
                }
                KeyCode::Right => {
                    let _ = self.main_tx.send(Msg2Sub::MoveForward);
                }
                KeyCode::Char('L') => {
                    let _ = self.main_tx.send(Msg2Sub::ToggleLoop);
                }
                KeyCode::Enter => {
                    if let Some(index) = &self.media.state.selected() {
                        let _ = self.main_tx.send(Msg2Sub::Select(*index));
                    }
                }
                _ => (),
            },
            Focus::Devices => match key.code {
                KeyCode::Esc => self.exit_reason = Some(ReasonStopped::Quit),
                KeyCode::Tab => self.exit_reason = Some(ReasonStopped::ChangeScreen),
                KeyCode::Char('h') => self.focus = Focus::Songs,
                KeyCode::Up | KeyCode::Char('k') => self.device_list_state.select_previous(),
                KeyCode::Down | KeyCode::Char('j') => self.device_list_state.select_next(),
                KeyCode::Char('g') | KeyCode::Home => self.device_list_state.select_first(),
                KeyCode::Char('G') | KeyCode::End => self.device_list_state.select_last(),
                KeyCode::Char('f') => {
                    if let Err(_) = self.send_devices(true) {
                        self.exit_reason = Some(ReasonStopped::NoOutputDevices);
                    }
                }
                KeyCode::Enter => self.switch_device(),
                KeyCode::Char('p') | KeyCode::F(8) | KeyCode::Pause | KeyCode::Char(' ') => {
                    let _ = self.main_tx.send(Msg2Sub::PauseUnpause);
                }
                KeyCode::Char('s') | KeyCode::F(9) => {
                    let _ = self.main_tx.send(Msg2Sub::Skip);
                }
                KeyCode::Char('m') | KeyCode::F(10) => {
                    let _ = self.main_tx.send(Msg2Sub::Mute);
                }
                KeyCode::Char('v') | KeyCode::F(11) => {
                    let _ = self.main_tx.send(Msg2Sub::DecreaseVolume);
                }
                KeyCode::Char('V') | KeyCode::F(12) => {
                    let _ = self.main_tx.send(Msg2Sub::IncreaseVolume);
                }
                KeyCode::Char('r') => self.refresh_devices(),
                KeyCode::Left => {
                    let _ = self.main_tx.send(Msg2Sub::MoveBackward);
                }
                KeyCode::Right => {
                    let _ = self.main_tx.send(Msg2Sub::MoveForward);
                }
                KeyCode::Char('L') => {
                    let _ = self.main_tx.send(Msg2Sub::ToggleLoop);
                }
                _ => (),
            },
        }
    }

    fn refresh_devices(&mut self) {
        self.devices = SongsState::get_devices();
        self.device_list_state = ListState::default();
    }

    fn get_devices() -> AvailableDevices {
        let mut output_devices = Vec::with_capacity(8);
        let host = cpal::default_host();

        if let Some(default_device) = host.default_output_device() {
            output_devices.push(default_device);
        }

        if let Ok(devices) = host.devices() {
            for device in devices {
                if device.supports_output() {
                    output_devices.push(device);
                }
            }
        }
        AvailableDevices::new(output_devices)
    }

    fn switch_device(&self) {
        if let Some(index) = self.device_list_state.selected() {
            if let Some(device) = self.devices.output_devices.get(index) {
                let _ = self.main_tx.send(Msg2Sub::SwitchDevice(device.clone()));
            }
        }
    }

    fn send_devices(&self, start_playback: bool) -> Result<(), AppError> {
        let available_devices = SongsState::get_devices();
        if available_devices.is_empty() {
            return Err(AppError::NoOutputDevices);
        }
        let _ = self
            .main_tx
            .send(Msg2Sub::Devices(available_devices, start_playback));
        Ok(())
    }
}

impl SongsState {
    fn render_header(area: Rect, buf: &mut Buffer) {
        Paragraph::new("Terminal Audio Player")
            .bold()
            .centered()
            .render(area, buf);
    }

    fn render_list(&mut self, area: Rect, buf: &mut Buffer) {
        let main_layout = Layout::horizontal([Constraint::Fill(4), Constraint::Fill(1)])
            .flex(Flex::Start)
            .spacing(4);
        let [left, right] = area.layout(&main_layout);

        let rhs_layout =
            Layout::vertical([Constraint::Max(16), Constraint::Max(3), Constraint::Max(1)])
                .spacing(4);
        let [r0, r1, r2] = right.layout(&rhs_layout);

        let block = Block::new().title(Line::raw("Songs").centered());
        let list_songs = List::new(self.media.sources.iter().map(|media_source| {
            let list_item = ListItem::from(media_source.file_name());
            if media_source.is_playing {
                list_item.style(LIST_ITEM_STYLE)
            } else {
                list_item
            }
        }))
        .block(block)
        .highlight_style(LIST_STYLE)
        .highlight_symbol(">")
        .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(list_songs, left, buf, &mut self.media.state);

        let block = Block::new().title(Line::raw("Devices").centered());
        let list_devices = List::new(self.devices.names.iter().enumerate().map(|(i, name)| {
            let list_item = ListItem::from(name.as_str());
            if i == self.devices.current_device {
                list_item.style(DEVICE_ITEM_STYLE)
            } else {
                list_item
            }
        }))
        .block(block)
        .highlight_style(LIST_STYLE)
        .highlight_symbol(">")
        .highlight_spacing(HighlightSpacing::Always);
        StatefulWidget::render(list_devices, r0, buf, &mut self.device_list_state);

        let playback_info = match (
            self.playback_position.current_pos,
            self.playback_position.total_duration,
        ) {
            (Some(pos), Some(total_duration)) => &format!(
                "{:.3}/{:.3}",
                pos.as_secs_f32(),
                total_duration.as_secs_f32()
            ),
            (Some(pos), None) => &format!("{:.3}/?", pos.as_secs_f32()),
            (None, Some(total_duration)) => &format!("?/{:.3}", total_duration.as_secs_f32()),
            (None, None) => "Unknown Playback Position",
        };

        let playback_text = Text::from(format!(
            "Volume: {}\nLooping: {}\n{}",
            (self.playback_position.volume * 10.0) as u8,
            self.playback_position.is_looping,
            playback_info,
        ));
        playback_text.render(r1, buf);

        if let Some((instant, msg)) = self.message {
            if Instant::now() - instant < Duration::from_millis(3000) {
                msg.render(r2, buf);
            } else {
                self.message = None;
            }
        }
    }
}

impl Widget for &mut SongsState {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let main_layout = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]);

        let [header_area, main_area] = area.layout(&main_layout);
        SongsState::render_header(header_area, buf);
        self.render_list(main_area, buf);
    }
}

struct PlaybackState {
    rng: rand::rngs::ThreadRng,
    sub_tx: Sender<Msg2Main>,
    main_rx: Receiver<Msg2Sub>,
    stream_handle: MixerDeviceSink,
    sink: Player,
    current_device: Device,
    status: Status,
    codec_registry: Arc<CodecRegistry>,
    index: usize,
    sources: Vec<PathBuf>,
    volume: f32,
    position: Option<Duration>,
    last_position_update: Instant,
    last_device_healthcheck: Instant,
    total_duration: Option<Duration>,
    should_loop: bool,
}

impl PlaybackState {
    fn new(
        sub_tx: Sender<Msg2Main>,
        main_rx: Receiver<Msg2Sub>,
        stream_handle: MixerDeviceSink,
        sink: Player,
        sources: Vec<PathBuf>,
        current_device: Device,
        status: Status,
        codec_registry: Arc<CodecRegistry>,
    ) -> Self {
        Self {
            rng: rand::rng(),
            sub_tx,
            main_rx,
            stream_handle,
            sink,
            current_device,
            status,
            codec_registry,
            index: 0,
            sources,
            volume: 0.8,
            position: None,
            last_position_update: Instant::now(),
            last_device_healthcheck: Instant::now(),
            total_duration: None,
            should_loop: false,
        }
    }

    fn source_from_file(&self) -> rodio::Decoder<BufReader<File>> {
        let file = File::open(&self.sources[self.index]).expect("open failed");

        let source = match DecoderBuilder::new()
            .with_codec_registry(self.codec_registry.clone())
            .with_data(BufReader::new(file))
            .build()
        {
            Ok(source) => source,
            Err(e) => {
                let _ = self
                    .sub_tx
                    .send(Msg2Main::Panic("unsupported or invalid format"));
                panic!("{}", e);
            }
        };
        source
    }

    // does not start playback
    fn _restore_state(&mut self, sink: &Player) {
        // restore previous settings
        sink.set_volume(self.volume);

        // re-add source
        let source = self.source_from_file();
        self.total_duration = source.total_duration();
        sink.append(source);

        // seek
        if let Some(pos) = self.position {
            let _ = sink.try_seek(pos);
        }
    }

    // NOTE: if switching devices fails, it's a no-op (besides printing a message)
    // failure would likely occur if the device disconnected
    fn switch_device(&mut self, device: Device) {
        match build_stream(device.clone(), self.sub_tx.clone()) {
            Ok(stream) => {
                self.current_device = device.clone();
                self.stream_handle = stream;
                let mixer = self.stream_handle.mixer();
                let sink = Player::connect_new(&mixer);
                self._restore_state(&sink);
                sink.play();

                self.sink = sink;
                self.status = Status::Playing;
                let _ = self.sub_tx.send(Msg2Main::DeviceSwitched(device.clone()));
            }
            Err(_e) => {
                let _ = self.sub_tx.send(Msg2Main::OutputStreamInitializationFailed);
            }
        }
    }

    fn rebuild_player_from_new_devices(&mut self, devices: AvailableDevices, start_playback: bool) {
        let mut stream = None;
        for device in devices.output_devices {
            match build_stream(device.clone(), self.sub_tx.clone()) {
                Ok(player) => {
                    stream = Some(player);
                    self.current_device = device.clone();
                }
                Err(_e) => continue,
            }
        }

        let new_sink = match stream {
            Some(stream) => {
                self.stream_handle = stream;
                let mixer = self.stream_handle.mixer();
                Player::connect_new(&mixer)
            }
            None => {
                let _ = self
                    .sub_tx
                    .send(Msg2Main::Panic("no output device available"));
                panic!("failed to create sink for any output device");
            }
        };
        self._restore_state(&new_sink);

        if start_playback {
            new_sink.play();
            self.status = Status::Playing;
        }

        self.sink = new_sink;
        let _ = self
            .sub_tx
            .send(Msg2Main::DeviceSwitched(self.current_device.clone()));
    }

    fn rebuild_player_from_current_device(&mut self) {
        let stream = match build_stream(self.current_device.clone(), self.sub_tx.clone()) {
            Ok(player) => player,
            Err(_e) => {
                let _ = self.sub_tx.send(Msg2Main::DeviceLostWait);
                return;
            }
        };

        self.stream_handle = stream;
        let mixer = self.stream_handle.mixer();
        let new_sink = Player::connect_new(&mixer);

        match self.status {
            Status::Paused => self._restore_state(&new_sink),
            Status::Waiting => {
                new_sink.set_volume(self.volume);
                self.sink = new_sink;
            }
            _ => unreachable!(),
        }

        self.last_device_healthcheck = Instant::now();
        let _ = self
            .sub_tx
            .send(Msg2Main::DeviceSwitched(self.current_device.clone()));
    }

    fn toggle_loop(&mut self) {
        self.should_loop = !self.should_loop;
        let _ = self.sub_tx.send(Msg2Main::LoopStatus(self.should_loop));
    }

    fn run(&mut self) -> Result<(), AppError> {
        loop {
            match self.status {
                Status::Startup => {
                    let _ = self.sub_tx.send(Msg2Main::SongChanged(Some(self.index)));

                    self.add_next_source()?;
                    self.sink.play();
                    self.status = Status::Playing;
                }
                Status::NextSong => {
                    if !self.should_loop {
                        self.index += 1;
                        if self.index >= self.sources.len() {
                            self.index = 0;
                            self.status = Status::Waiting;

                            let _ = self.sub_tx.send(Msg2Main::SongChanged(None));
                            continue;
                        }

                        let _ = self.sub_tx.send(Msg2Main::SongChanged(Some(self.index)));
                    }

                    self.add_next_source()?;
                    self.sink.play();
                    self.status = Status::Playing;
                }
                Status::Playing => {
                    if self.sink.empty() {
                        self.status = Status::NextSong;
                        self.total_duration = None;
                        continue;
                    }
                    self.update_position();
                    self.last_device_healthcheck = Instant::now();
                    match self.main_rx.try_recv() {
                        Ok(msg) => match msg {
                            Msg2Sub::PauseUnpause => self.pause_unpause(),
                            Msg2Sub::Skip => self.skip(),
                            Msg2Sub::Select(index) => self.select_source(index),
                            Msg2Sub::DecreaseVolume => self.decrease_volume(),
                            Msg2Sub::IncreaseVolume => self.increase_volume(),
                            Msg2Sub::Mute => self.mute(),
                            Msg2Sub::NewSources(new_sources) => self.new_sources(new_sources),
                            Msg2Sub::Randomize => self.randomize_sources(),
                            Msg2Sub::MoveForward => self.try_move_forward(),
                            Msg2Sub::MoveBackward => self.try_move_backward(),
                            Msg2Sub::ShouldExit => self.status = Status::ShouldExit,
                            Msg2Sub::SwitchDevice(device) => self.switch_device(device),
                            Msg2Sub::Devices(ds, start_playback) => {
                                self.rebuild_player_from_new_devices(ds, start_playback)
                            }
                            Msg2Sub::ToggleLoop => self.toggle_loop(),
                        },
                        Err(e) => match e {
                            TryRecvError::Empty => (),
                            TryRecvError::Disconnected => break,
                        },
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Status::Paused => {
                    if Instant::now() - self.last_device_healthcheck > Duration::from_secs(60) {
                        self.rebuild_player_from_current_device();
                    }
                    match self.main_rx.try_recv() {
                        Ok(msg) => match msg {
                            Msg2Sub::PauseUnpause => self.pause_unpause(),
                            Msg2Sub::Skip => self.skip(),
                            Msg2Sub::Select(index) => self.select_source(index),
                            Msg2Sub::NewSources(new_sources) => self.new_sources(new_sources),
                            Msg2Sub::IncreaseVolume => self.increase_volume(),
                            Msg2Sub::DecreaseVolume => self.decrease_volume(),
                            Msg2Sub::Mute => self.mute(),
                            Msg2Sub::Randomize => self.randomize_sources(),
                            Msg2Sub::MoveForward | Msg2Sub::MoveBackward => (),
                            Msg2Sub::ShouldExit => self.status = Status::ShouldExit,
                            Msg2Sub::SwitchDevice(device) => self.switch_device(device),
                            Msg2Sub::Devices(ds, start_playback) => {
                                self.rebuild_player_from_new_devices(ds, start_playback)
                            }
                            Msg2Sub::ToggleLoop => self.toggle_loop(),
                        },
                        Err(e) => match e {
                            TryRecvError::Empty => (),
                            TryRecvError::Disconnected => break,
                        },
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                // reached end of the sources
                Status::Waiting => {
                    if Instant::now() - self.last_device_healthcheck > Duration::from_secs(60) {
                        self.rebuild_player_from_current_device();
                    }
                    match self.main_rx.try_recv() {
                        Ok(msg) => match msg {
                            Msg2Sub::PauseUnpause | Msg2Sub::Skip => (),
                            Msg2Sub::Select(index) => self.select_source(index),
                            Msg2Sub::DecreaseVolume => self.decrease_volume(),
                            Msg2Sub::IncreaseVolume => self.increase_volume(),
                            Msg2Sub::Mute => self.mute(),
                            Msg2Sub::NewSources(new_sources) => self.new_sources(new_sources),
                            Msg2Sub::Randomize => self.randomize_sources(),
                            Msg2Sub::MoveForward | Msg2Sub::MoveBackward => (),
                            Msg2Sub::ShouldExit => self.status = Status::ShouldExit,
                            Msg2Sub::SwitchDevice(device) => self.switch_device(device),
                            Msg2Sub::Devices(ds, start_playback) => {
                                self.rebuild_player_from_new_devices(ds, start_playback)
                            }
                            Msg2Sub::ToggleLoop => self.toggle_loop(),
                        },
                        Err(e) => match e {
                            TryRecvError::Empty => (),
                            TryRecvError::Disconnected => break,
                        },
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Status::ShouldExit => break,
            }
        }
        Ok(())
    }

    fn update_position(&mut self) {
        if !self.sink.empty() {
            let pos = self.sink.get_pos();
            self.position = Some(pos);
        } else {
            self.position = None;
        }
        let now = Instant::now();
        if now - self.last_position_update >= Duration::from_millis(1000) {
            let _ = self.sub_tx.send(Msg2Main::UpdatePosition(self.position));
            self.last_position_update = now;
        }
    }

    fn mute(&mut self) {
        self.sink.set_volume(0.0);
        self.volume = 0.0;
        let _ = self.sub_tx.send(Msg2Main::VolumeChanged(0.0));
    }

    fn increase_volume(&mut self) {
        let new_volume = (((self.volume * 10.0) as u8 + 1) as f32 / 10.0).clamp(0.0, 1.0);
        self.volume = new_volume;
        self.sink.set_volume(new_volume);
        let _ = self.sub_tx.send(Msg2Main::VolumeChanged(new_volume));
    }

    fn decrease_volume(&mut self) {
        let new_volume =
            (((self.volume * 10.0) as u8).saturating_sub(1) as f32 / 10.0).clamp(0.0, 1.0);
        self.volume = new_volume;
        self.sink.set_volume(new_volume);
        let _ = self.sub_tx.send(Msg2Main::VolumeChanged(new_volume));
    }

    fn select_source(&mut self, index: usize) {
        self.sink.clear();
        self.index = index;
        self.status = Status::Startup;
    }

    fn skip(&mut self) {
        self.sink.clear();
        self.status = Status::NextSong;
    }

    fn new_sources(&mut self, new_sources: Vec<PathBuf>) {
        self.sink.clear();
        self.sources.clear();
        self.sources = new_sources;
        self.index = 0;
        self.status = Status::Startup;
    }

    fn add_next_source(&mut self) -> Result<(), AppError> {
        let source = self.source_from_file();
        self.total_duration = source.total_duration();
        self.sink.append(source);
        let _ = self
            .sub_tx
            .send(Msg2Main::TotalDuration(self.total_duration));
        Ok(())
    }

    fn pause_unpause(&mut self) {
        if self.sink.is_paused() {
            self.sink.play();
            self.status = Status::Playing;
        } else {
            self.sink.pause();
            self.status = Status::Paused;
        }
    }

    fn randomize_sources(&mut self) {
        use rand::seq::SliceRandom;

        self.sink.clear();
        self.sources.shuffle(&mut self.rng);
        self.index = 0;
        self.status = Status::Startup;
        let _ = self.sub_tx.send(Msg2Main::Randomized(self.sources.clone()));
    }

    fn try_move_forward(&mut self) {
        if let Some(pos) = self.position {
            let _ = self.sink.try_seek(pos + Duration::from_secs(1));
        }
    }

    fn try_move_backward(&mut self) {
        if let Some(pos) = self.position {
            let _ = self.sink.try_seek(pos - Duration::from_secs(1));
        }
    }
}

fn build_stream(device: Device, tx: Sender<Msg2Main>) -> Result<MixerDeviceSink, AppError> {
    let mut stream = DeviceSinkBuilder::from_device(device)
        .map_err(AppError::Sink)?
        .with_error_callback(move |err: cpal::StreamError| match err {
            cpal::StreamError::DeviceNotAvailable => {
                let _ = tx.send(Msg2Main::DeviceLost);
            }
            _ => {
                let _ = tx.send(Msg2Main::PanicS(err.to_string()));
                panic!("{}", err);
            }
        })
        .open_sink_or_fallback()
        .map_err(AppError::Sink)?;
    stream.log_on_drop(false);

    Ok(stream)
}

#[derive(Default, PartialEq, Eq)]
enum DirectoryFocus {
    #[default]
    View,
    Textedit,
}

struct Debounce {
    last_trigger: Option<Instant>,
    delay: Duration,
}

impl Debounce {
    fn trigger(&mut self) {
        self.last_trigger = Some(Instant::now());
    }

    fn ready(&self) -> bool {
        if let Some(ts) = self.last_trigger {
            Instant::now() - ts > self.delay
        } else {
            false
        }
    }
}

impl Default for Debounce {
    fn default() -> Self {
        Self {
            last_trigger: None,
            delay: Duration::from_millis(100),
        }
    }
}

struct DirectoryState {
    main_tx: Sender<Msg2Sub>,
    root_dir: String,
    textedit: String,
    items: DirItems,
    filtered_items: DirItems,
    contents: HashMap<PathBuf, Vec<String>>,
    focus: DirectoryFocus,
    debounce: Debounce,
    exit_reason: Option<ReasonStopped>,
}

impl DirectoryState {
    fn new(main_tx: Sender<Msg2Sub>, root_dir: &str) -> Self {
        let mut state = Self {
            main_tx,
            root_dir: root_dir.to_owned(),
            textedit: String::with_capacity(32),
            items: DirItems {
                dirs: Vec::with_capacity(128),
                state: ListState::default(),
            },
            filtered_items: DirItems {
                dirs: Vec::with_capacity(128),
                state: ListState::default(),
            },
            contents: HashMap::new(),
            focus: DirectoryFocus::default(),
            debounce: Debounce::default(),
            exit_reason: None,
        };
        state.update_candidates();
        state
    }

    fn handle_key(&mut self, event: KeyEvent) {
        match self.focus {
            DirectoryFocus::View => match event.code {
                KeyCode::Esc => self.exit_reason = Some(ReasonStopped::Quit),
                KeyCode::Tab => self.exit_reason = Some(ReasonStopped::ChangeScreen),
                KeyCode::Char('/') => self.focus = DirectoryFocus::Textedit,
                KeyCode::Up | KeyCode::Char('k') => self.items.state.select_previous(),
                KeyCode::Down | KeyCode::Char('j') => self.items.state.select_next(),
                KeyCode::Char('g') | KeyCode::Home => self.items.state.select_first(),
                KeyCode::Char('G') | KeyCode::End => self.items.state.select_last(),
                KeyCode::Enter => self.exit_reason = Some(ReasonStopped::NewSources),
                KeyCode::Char('p') | KeyCode::F(8) | KeyCode::Pause | KeyCode::Char(' ') => {
                    let _ = self.main_tx.send(Msg2Sub::PauseUnpause);
                }
                KeyCode::Char('r') => {
                    let _ = self.main_tx.send(Msg2Sub::Randomize);
                }
                KeyCode::Char('s') | KeyCode::F(9) => {
                    let _ = self.main_tx.send(Msg2Sub::Skip);
                }
                KeyCode::Char('m') | KeyCode::F(10) => {
                    let _ = self.main_tx.send(Msg2Sub::Mute);
                }
                KeyCode::Char('v') | KeyCode::F(11) => {
                    let _ = self.main_tx.send(Msg2Sub::DecreaseVolume);
                }
                KeyCode::Char('V') | KeyCode::F(12) => {
                    let _ = self.main_tx.send(Msg2Sub::IncreaseVolume);
                }
                KeyCode::Left => {
                    let _ = self.main_tx.send(Msg2Sub::MoveBackward);
                }
                KeyCode::Right => {
                    let _ = self.main_tx.send(Msg2Sub::MoveForward);
                }
                KeyCode::Char('L') => {
                    let _ = self.main_tx.send(Msg2Sub::ToggleLoop);
                }
                _ => (),
            },
            DirectoryFocus::Textedit => match event.code {
                KeyCode::Esc => self.exit_reason = Some(ReasonStopped::Quit),
                KeyCode::Tab => self.focus = DirectoryFocus::View,
                KeyCode::Char(c) if event.modifiers == crossterm::event::KeyModifiers::NONE => {
                    self.textedit.push(c);
                    self.debounce.trigger();
                }
                KeyCode::Backspace => {
                    self.textedit.pop();
                    self.debounce.trigger();
                }
                KeyCode::Char(c) if event.modifiers == crossterm::event::KeyModifiers::CONTROL => {
                    match c {
                        'j' => self.filtered_items.state.select_next(),
                        'k' => self.filtered_items.state.select_previous(),
                        'f' => self.filtered_items.state.select_first(),
                        'l' => self.filtered_items.state.select_last(),
                        _ => (),
                    }
                }
                KeyCode::Up => self.filtered_items.state.select_previous(),
                KeyCode::Down => self.filtered_items.state.select_next(),
                KeyCode::Enter => {
                    self.exit_reason = Some(ReasonStopped::NewSources);
                    self.textedit.clear();
                }
                _ => (),
            },
        }
    }

    fn filter_choices(&mut self) {
        let filtered = self
            .items
            .dirs
            .iter()
            .cloned()
            .filter(|d| d.dir_without_root_string.contains(&self.textedit));
        self.filtered_items.dirs = filtered.collect();
    }

    fn update_candidates(&mut self) {
        let mut v = Vec::with_capacity(64);
        find_dirs_with_music(&self.root_dir, &mut v);

        for d in &v {
            let mut dir = fs::read_dir(&d).expect("already read before");
            while let Some(Ok(entry)) = dir.next() {
                if let Ok(file_type) = entry.file_type() {
                    if file_type.is_file() && matches_file_extensions(&entry) {
                        self.contents
                            .entry(d.to_path_buf())
                            .or_insert(Vec::with_capacity(32))
                            .push(entry.file_name().to_str().unwrap_or_default().to_owned());
                    }
                }
            }
            self.contents.get_mut(d).unwrap().sort();
        }

        let mut dir_items: Vec<DirItem> = v
            .into_iter()
            .map(|d| DirItem::new(d, &self.root_dir))
            .collect();
        dir_items.sort();
        self.items.dirs = dir_items;
    }
}

impl DirectoryState {
    fn render_header(area: Rect, buf: &mut Buffer) {
        Paragraph::new("Music Folders")
            .bold()
            .centered()
            .render(area, buf)
    }

    fn render_choices(&mut self, area: Rect, buf: &mut Buffer) {
        let items: Vec<ListItem> = match self.focus {
            DirectoryFocus::View => self
                .items
                .dirs
                .iter()
                .map(|i| ListItem::from(i.dir_without_root_string.as_str()))
                .collect(),
            DirectoryFocus::Textedit => self
                .filtered_items
                .dirs
                .iter()
                .map(|i| ListItem::from(i.dir_without_root_string.as_str()))
                .collect(),
        };

        let block = Block::new();

        let layout = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(2)])
            .flex(Flex::Start)
            .spacing(4);
        let [left, right] = area.layout(&layout);

        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(SLATE.c800).add_modifier(Modifier::BOLD))
            .highlight_symbol(">")
            .highlight_spacing(HighlightSpacing::Always);

        match self.focus {
            DirectoryFocus::Textedit => {
                let layout = Layout::vertical([Constraint::Max(1), Constraint::Fill(1)]);
                let [top_left, bottom_left] = left.layout(&layout);

                let line = Line::from_iter(["> ", self.textedit.as_str()]).bold();
                line.render(top_left, buf);
                StatefulWidget::render(list, bottom_left, buf, &mut self.filtered_items.state);

                if let Some(index) = self.filtered_items.state.selected() {
                    if let Some(contents) =
                        self.contents.get(&self.filtered_items.dirs[index].full_dir)
                    {
                        let c = contents.iter().map(|d| ListItem::from(d.as_str()));
                        let list = List::new(c);
                        Widget::render(list, right, buf);
                    }
                }
            }
            DirectoryFocus::View => {
                StatefulWidget::render(list, left, buf, &mut self.items.state);

                if let Some(index) = self.items.state.selected() {
                    if let Some(contents) = self.contents.get(&self.items.dirs[index].full_dir) {
                        let c = contents.iter().map(|d| ListItem::from(d.as_str()));
                        let list = List::new(c);
                        Widget::render(list, right, buf);
                    }
                }
            }
        }
    }
}

impl Widget for &mut DirectoryState {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let main_layout = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]);

        let [header_area, main_area] = area.layout(&main_layout);
        DirectoryState::render_header(header_area, buf);

        self.render_choices(main_area, buf);
    }
}

struct RootState {
    songs_state: SongsState,
    directory_state: DirectoryState,
    current_state: CurrentState,
    should_quit: bool,
}

impl RootState {
    fn new() -> Self {
        let args = Args::parse();

        let devices = SongsState::get_devices();
        if devices.is_empty() {
            eprintln!("there are no output devices");
            std::process::exit(1);
        }

        let mut sources = Vec::with_capacity(32);
        let (initial_status, initial_state) = match &args.dir {
            Some(dir) => {
                if let Ok(mut read_dir) = fs::read_dir(&dir) {
                    while let Some(Ok(entry)) = read_dir.next() {
                        if entry.path().extension().is_some_and(|ext| {
                            FILE_EXTENSIONS.contains(&ext.to_string_lossy().as_ref())
                        }) {
                            sources.push(MediaSource {
                                path: entry.path(),
                                is_playing: false,
                            });
                        }
                    }
                }
                if sources.is_empty() {
                    eprintln!("there's nothing to play");
                    std::process::exit(1);
                }
                sources.sort();
                sources[0].is_playing = true;
                (Status::Startup, CurrentState::Songs)
            }
            None => (Status::Waiting, CurrentState::Search),
        };

        let (main_tx, main_rx): (Sender<Msg2Sub>, Receiver<Msg2Sub>) = mpsc::channel();

        Self {
            songs_state: SongsState::new(
                main_tx.clone(),
                main_rx,
                sources,
                devices,
                initial_status,
            ),
            directory_state: DirectoryState::new(main_tx, &args.root_dir),
            current_state: initial_state,
            should_quit: false,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<(), AppError> {
        while !self.should_quit {
            match self.current_state {
                CurrentState::Songs => {
                    let exit_reason = loop {
                        match self.songs_state.exit_reason.take() {
                            Some(reason) => break reason,
                            None => (),
                        }

                        self.songs_state.read_channel();

                        terminal
                            .draw(|frame| frame.render_widget(&mut self.songs_state, frame.area()))
                            .map_err(|_e| AppError::DrawFrame)?;

                        if event::poll(Duration::from_millis(100)).map_err(AppError::Io)? {
                            if let Some(key) =
                                event::read().map_err(AppError::Io)?.as_key_press_event()
                            {
                                self.songs_state.handle_key(key);
                            }
                        }
                    };

                    match exit_reason {
                        ReasonStopped::ChangeScreen => self.current_state = CurrentState::Search,
                        ReasonStopped::Quit => self.should_quit = true,
                        _ => unreachable!(),
                    }
                }
                CurrentState::Search => {
                    let exit_reason = loop {
                        match self.directory_state.exit_reason.take() {
                            Some(reason) => break reason,
                            None => (),
                        }

                        if self.directory_state.debounce.ready() {
                            self.directory_state.filter_choices();
                        }

                        terminal
                            .draw(|frame| {
                                frame.render_widget(&mut self.directory_state, frame.area())
                            })
                            .map_err(|_e| AppError::DrawFrame)?;

                        if event::poll(Duration::from_millis(100)).map_err(AppError::Io)? {
                            if let Some(key) =
                                event::read().map_err(AppError::Io)?.as_key_press_event()
                            {
                                self.directory_state.handle_key(key);
                            }
                        }
                    };

                    match exit_reason {
                        ReasonStopped::ChangeScreen => self.current_state = CurrentState::Songs,
                        ReasonStopped::Quit => self.should_quit = true,
                        ReasonStopped::NoOutputDevices => {
                            return Err(AppError::NoOutputDevices);
                        }
                        ReasonStopped::NewSources => {
                            let dir = match self.directory_state.focus {
                                DirectoryFocus::View => self
                                    .directory_state
                                    .items
                                    .state
                                    .selected()
                                    .map(|i| &self.directory_state.items.dirs[i].full_dir),
                                DirectoryFocus::Textedit => self
                                    .directory_state
                                    .filtered_items
                                    .state
                                    .selected()
                                    .map(|i| &self.directory_state.filtered_items.dirs[i].full_dir),
                            };
                            if let Some(dir) = dir {
                                let mut sources = Vec::with_capacity(32);
                                if let Ok(mut read_dir) = fs::read_dir(&dir) {
                                    while let Some(Ok(entry)) = read_dir.next() {
                                        if matches_file_extensions(&entry) {
                                            sources.push(MediaSource {
                                                path: entry.path(),
                                                is_playing: false,
                                            });
                                        }
                                    }
                                }
                                assert!(!sources.is_empty());
                                sources.sort();
                                sources[0].is_playing = true;

                                let new_paths = sources.iter().map(|s| s.path.clone()).collect();
                                let new_media_sources = MediaSources {
                                    sources,
                                    state: ListState::default(),
                                };
                                self.songs_state.media = new_media_sources;

                                let _ = self
                                    .songs_state
                                    .main_tx
                                    .send(Msg2Sub::NewSources(new_paths));
                                // Drain the channel when switching sources
                                // After each song, Msg2Main::SongChanged is sent to the main thread
                                // There is a time window where which we change sources,
                                // setting the song index to 0 but the message remains in the channel,
                                // which is processed when going back to CurrentState::Songs,
                                // causing the terminal styling to be incorrect
                                for _ in self.songs_state.sub_rx.try_iter() {}
                                self.current_state = CurrentState::Songs;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn main() -> Result<(), AppError> {
    let mut state = RootState::new();

    let mut terminal = ratatui::init();
    let result = state.run(&mut terminal);

    // attempt to stop audio thread
    let _ = state.songs_state.main_tx.send(Msg2Sub::ShouldExit);

    // prevent blocking indefinitely if the child thread does not exit
    let jh = state.songs_state.playback_jh;
    let mut attempts = 4;
    loop {
        if attempts == 0 {
            break;
        }
        if jh.is_finished() {
            let _ = jh.join();
            break;
        }
        attempts -= 1;
        thread::sleep(Duration::from_millis(100));
    }

    ratatui::restore();
    result
}

fn matches_file_extensions(entry: &fs::DirEntry) -> bool {
    entry.path().extension().is_some_and(|ext| {
        ext.to_str()
            .is_some_and(|ext| FILE_EXTENSIONS.contains(&ext))
    })
}

fn find_dirs_with_music<P: AsRef<Path>>(start: P, v: &mut Vec<PathBuf>) {
    if let Ok(mut dir) = fs::read_dir(start.as_ref()) {
        while let Some(Ok(entry)) = dir.next() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    if let Ok(mut inner_dir) = fs::read_dir(&entry.path()) {
                        while let Some(Ok(inner_entry)) = inner_dir.next() {
                            if let Ok(file_type) = inner_entry.file_type() {
                                if file_type.is_dir() {
                                    find_dirs_with_music(inner_entry.path(), v);
                                } else if file_type.is_file() {
                                    if matches_file_extensions(&inner_entry) {
                                        let p = entry.path();
                                        if !v.contains(&p) {
                                            v.push(p);
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if file_type.is_file() {
                    if matches_file_extensions(&entry) {
                        let p = start.as_ref().to_path_buf();
                        if !v.contains(&p) {
                            v.push(p);
                        }
                    }
                }
            }
        }
    }
}

enum CurrentState {
    Songs,
    Search,
}

#[derive(Clone, Copy)]
enum ReasonStopped {
    Quit,
    NoOutputDevices,
    ChangeScreen,
    NewSources,
}

enum Status {
    Startup,
    Paused,
    Playing,
    NextSong,
    Waiting,
    ShouldExit,
}

enum Msg2Main {
    SongChanged(Option<usize>),
    Randomized(Vec<PathBuf>),
    TotalDuration(Option<Duration>),
    UpdatePosition(Option<Duration>),
    VolumeChanged(f32),
    DeviceLost,
    DeviceLostWait,
    DeviceSwitched(Device),
    Panic(&'static str),
    PanicS(String),
    OutputStreamInitializationFailed,
    LoopStatus(bool),
}

enum Msg2Sub {
    PauseUnpause,
    Skip,
    Select(usize),
    IncreaseVolume,
    DecreaseVolume,
    Mute,
    NewSources(Vec<PathBuf>),
    Randomize,
    MoveForward,
    MoveBackward,
    ShouldExit,
    ToggleLoop,
    SwitchDevice(Device),
    Devices(AvailableDevices, bool),
}

/// Store absolute path,
/// path stripped relative to root music folder for rendering,
/// and String stripped path for filtering
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, std::hash::Hash)]
struct DirItem {
    full_dir: PathBuf,
    dir_without_root: PathBuf,
    dir_without_root_string: String,
}

impl DirItem {
    fn new<P: AsRef<Path>>(dir: PathBuf, root: P) -> Self {
        let without_root = dir
            .strip_prefix(root.as_ref())
            .expect("cannot strip prefx")
            .to_path_buf();
        let without_root_as_str = without_root.to_str().unwrap_or_default().to_owned();

        Self {
            full_dir: dir,
            dir_without_root: without_root,
            dir_without_root_string: without_root_as_str,
        }
    }
}

#[derive(Debug)]
struct DirItems {
    dirs: Vec<DirItem>,
    state: ListState,
}

#[derive(Clone)]
struct MediaSources {
    sources: Vec<MediaSource>,
    state: ListState,
}

#[derive(Debug, PartialOrd, Ord, PartialEq, Eq, Clone)]
struct MediaSource {
    path: PathBuf,
    is_playing: bool,
}

impl MediaSource {
    fn file_name(&self) -> &str {
        self.path
            .file_name()
            .expect("no filename")
            .to_str()
            .unwrap_or_default()
    }
}

struct Args {
    root_dir: String,
    dir: Option<String>,
}

impl Args {
    fn parse() -> Self {
        let mut args = env::args();
        let _ = args.next();

        let root_dir_path = env::home_dir()
            .expect("platform does not have a home directory")
            .join(".aup/root_dir.txt");

        let root_dir = match fs::read_to_string(&root_dir_path) {
            Ok(s) => s.trim().to_owned(),
            Err(e) => match e.kind() {
                std::io::ErrorKind::NotFound => {
                    eprintln!(
                        "Create {:?} with 1 line - the path to your root music folder",
                        &root_dir_path
                    );
                    std::process::exit(1);
                }
                _ => panic!("{}", e),
            },
        };

        let dir = args.next();
        Args { root_dir, dir }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
enum AppError {
    DrawFrame,
    NoOutputDevices,
    Io(std::io::Error),
    Sink(rodio::stream::DeviceSinkError),
    Decoder(rodio::decoder::DecoderError),
    Seek(rodio::source::SeekError),
    BuildStream(cpal::BuildStreamError),
    OutputConfig(cpal::DefaultStreamConfigError),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DrawFrame => write!(f, "failed to draw frame"),
            Self::Io(e) => write!(f, "{}", e),
            Self::Sink(e) => write!(f, "{}", e),
            Self::Decoder(e) => write!(f, "{}", e),
            Self::Seek(e) => write!(f, "{}", e),
            Self::BuildStream(e) => write!(f, "{}", e),
            Self::OutputConfig(e) => write!(f, "{}", e),
            Self::NoOutputDevices => write!(f, "no output devices"),
        }
    }
}

#[cfg(test)]
mod test {
    fn add_01(a: f32) -> f32 {
        std::cmp::min(10, (a * 10.0) as u8 + 1) as f32 / 10.0
    }

    #[test]
    fn sort_stuff() {
        let v = vec![
            "celeste",
            "dk_country/restored",
            "psyqui",
            "psyqui/your_voice_so",
        ];
        assert!(v.is_sorted());
    }

    #[test]
    fn increase_vol() {
        let initial_volume = 0.5;
        let new_volume = add_01(initial_volume);
        let new_volume = add_01(new_volume);
        let new_volume = add_01(new_volume);
        let new_volume = add_01(new_volume);
        let new_volume = add_01(new_volume);
        assert_eq!(new_volume, 1.0);
    }

    #[test]
    fn decrease_vol() {
        let initial_volume = 0.5;
        let new_volume = std::cmp::max(0, (initial_volume * 10.0) as u8 - 1) as f32 / 10.0;
        assert_eq!(new_volume, 0.4);
    }
}
