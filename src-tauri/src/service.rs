use crate::{
    audio::{self, CaptureEvent},
    codex::Backend,
    platform::{
        capture::{self, Capture},
        files::private_dir,
    },
    session::{self, Segment, Session, Settings, Status},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
};

type SharedSession = Arc<Mutex<Session>>;
type Notify = Arc<dyn Fn(&str, Value) + Send + Sync>;
pub struct Service {
    pub root: PathBuf,
    settings: Mutex<Settings>,
    sessions: Mutex<BTreeMap<String, SharedSession>>,
    current: Mutex<Option<String>>,
    capture: Mutex<Option<Capture>>,
    scheduler: Mutex<()>,
    active_transcripts: AtomicUsize,
    active_answers: AtomicUsize,
    samples: AtomicU64,
    peak: AtomicU32,
    error: Mutex<Option<String>>,
    backend: Arc<dyn Backend>,
    notify: Notify,
}
#[derive(Clone, Copy)]
enum Kind {
    Transcribe,
    Answer,
}
struct Task {
    session: SharedSession,
    id: u64,
    kind: Kind,
    audio: PathBuf,
    context: String,
    prompt: String,
    model: String,
    reasoning_effort: String,
    speed: String,
}

impl Service {
    pub fn new(
        root: PathBuf,
        backend: Arc<dyn Backend>,
        notify: Notify,
    ) -> Result<Arc<Self>, String> {
        private_dir(&root)?;
        private_dir(&root.join("sessions"))?;
        let settings = if root.join("settings.json").exists() {
            serde_json::from_slice::<Settings>(
                &fs::read(root.join("settings.json")).map_err(|e| e.to_string())?,
            )
            .map_err(|e| format!("Повреждены настройки: {e}"))?
        } else {
            Settings::default()
        };
        settings.validate()?;
        Ok(Arc::new(Self {
            root,
            settings: Mutex::new(settings),
            sessions: Mutex::new(BTreeMap::new()),
            current: Mutex::new(None),
            capture: Mutex::new(None),
            scheduler: Mutex::new(()),
            active_transcripts: AtomicUsize::new(0),
            active_answers: AtomicUsize::new(0),
            samples: AtomicU64::new(0),
            peak: AtomicU32::new(0),
            error: Mutex::new(None),
            backend,
            notify,
        }))
    }
    pub fn settings(&self) -> Settings {
        self.settings
            .lock()
            .expect("settings mutex poisoned")
            .clone()
    }
    pub fn save_settings(&self, mut settings: Settings) -> Result<(), String> {
        let mut current = self.settings.lock().map_err(|e| e.to_string())?;
        // Response controls save independently from the settings dialog.
        settings.model = current.model.clone();
        settings.reasoning_effort = current.reasoning_effort.clone();
        settings.speed = current.speed.clone();
        settings.validate()?;
        session::atomic_json(&self.root.join("settings.json"), &settings)?;
        *current = settings;
        drop(current);
        self.changed();
        Ok(())
    }
    pub fn save_model_settings(
        &self,
        model: String,
        reasoning_effort: String,
        speed: String,
    ) -> Result<(), String> {
        let mut current = self.settings.lock().map_err(|e| e.to_string())?;
        let mut updated = current.clone();
        updated.model = model;
        updated.reasoning_effort = reasoning_effort;
        updated.speed = speed;
        updated.validate()?;
        session::atomic_json(&self.root.join("settings.json"), &updated)?;
        *current = updated;
        drop(current);
        self.changed();
        Ok(())
    }
    pub fn snapshot(&self) -> Value {
        let id = self.current.lock().expect("current mutex poisoned").clone();
        let target = id
            .as_ref()
            .and_then(|id| self.sessions.lock().ok()?.get(id).cloned());
        let session = target.and_then(|s| s.lock().ok().map(|s| s.clone()));
        let recording = self
            .capture
            .lock()
            .expect("capture mutex poisoned")
            .is_some();
        let samples = if recording {
            self.samples.load(Ordering::Relaxed)
        } else {
            session
                .as_ref()
                .and_then(|s| s.segments.last())
                .and_then(|s| s.end_sample)
                .unwrap_or(0)
        };
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .expect("sessions mutex poisoned")
            .keys()
            .cloned()
            .collect();
        json!({"settings":self.settings(),"session":session,"sessions":sessions,"recording":recording,"samples":samples,"peak":f32::from_bits(self.peak.load(Ordering::Relaxed)),"error":self.error.lock().expect("error mutex poisoned").clone(),"data_dir":self.root})
    }
    fn changed(&self) {
        (self.notify)("state", self.snapshot());
    }
    pub fn set_error(&self, error: String) {
        *self.error.lock().expect("error mutex poisoned") = Some(error);
        self.changed();
    }
    pub fn clear_error(&self) {
        *self.error.lock().expect("error mutex poisoned") = None;
        self.changed();
    }
    fn update(
        &self,
        target: &SharedSession,
        change: impl FnOnce(&mut Session) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut state = target.lock().map_err(|e| e.to_string())?;
        change(&mut state)?;
        session::save(&self.root, &state)
    }
    pub fn select(self: &Arc<Self>, id: &str) -> Result<(), String> {
        if self.capture.lock().map_err(|e| e.to_string())?.is_some() {
            return Err("Сначала остановите запись текущей сессии.".into());
        }
        if !self
            .sessions
            .lock()
            .map_err(|e| e.to_string())?
            .contains_key(id)
        {
            return Err("Эта сессия не открыта в текущем запуске приложения.".into());
        }
        *self.current.lock().map_err(|e| e.to_string())? = Some(id.into());
        self.changed();
        self.schedule();
        Ok(())
    }
    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        let mut capture = self.capture.lock().map_err(|e| e.to_string())?;
        if capture.is_some() {
            return Err("Запись уже идёт.".into());
        }
        let mut settings = self.settings();
        let devices = capture::devices()?;
        if settings.microphone.is_empty() {
            settings.microphone = devices.default_microphone;
        }
        if settings.output.is_empty() {
            settings.output = devices.default_output;
        }
        let mut state = Session::new(&settings);
        state.recording = true;
        state.segments.push(Segment::recording(1, 0));
        session::save(&self.root, &state)?;
        let id = state.id.clone();
        let target = Arc::new(Mutex::new(state));
        let (tx, rx) = mpsc::channel();
        let recorder =
            match capture::start(self.root.join("sessions").join(&id), settings, 1, 0, tx) {
                Ok(r) => r,
                Err(error) => {
                    self.update(&target, |s| {
                        s.recording = false;
                        s.segments.clear();
                        Ok(())
                    })?;
                    return Err(error);
                }
            };
        self.sessions
            .lock()
            .map_err(|e| e.to_string())?
            .insert(id.clone(), target.clone());
        *self.current.lock().map_err(|e| e.to_string())? = Some(id);
        self.samples.store(0, Ordering::Relaxed);
        self.peak.store(0, Ordering::Relaxed);
        *capture = Some(recorder);
        drop(capture);
        let service = self.clone();
        thread::spawn(move || {
            for event in rx {
                service.capture_event(&target, event);
            }
        });
        self.clear_error();
        Ok(())
    }
    pub fn cut(&self, action_id: Option<&str>) -> Result<(), String> {
        let request = action_id
            .map(|id| self.settings().answer_options(id))
            .transpose()?;
        self.capture
            .lock()
            .map_err(|e| e.to_string())?
            .as_ref()
            .ok_or("Сначала нажмите «Начать».")?
            .cut(request)
    }
    pub fn stop(&self) -> Result<(), String> {
        self.capture
            .lock()
            .map_err(|e| e.to_string())?
            .as_ref()
            .ok_or("Запись не запущена.")?
            .stop()
    }
    fn capture_event(self: &Arc<Self>, target: &SharedSession, event: CaptureEvent) {
        match event {
            CaptureEvent::Level { samples, peak } => {
                self.samples.store(samples, Ordering::Relaxed);
                self.peak.store(peak.to_bits(), Ordering::Relaxed);
                (self.notify)("level", json!({"samples":samples,"peak":peak}));
            }
            CaptureEvent::Cut {
                seq,
                end_sample,
                request,
                continuing,
            } => {
                let result = self.update(target, |state| {
                    let segment = state.segment_mut(seq)?;
                    segment.end_sample = Some(end_sample);
                    if let Some(request) = request {
                        state.add_answer(seq, request)?;
                    }
                    if continuing {
                        state.segments.push(Segment::recording(seq + 1, end_sample));
                    }
                    Ok(())
                });
                if let Err(e) = result {
                    self.set_error(e);
                }
                self.changed();
                self.schedule();
            }
            CaptureEvent::Empty { request } => {
                let result = self.update(target, |state| {
                    let seq = state
                        .segments
                        .iter()
                        .rev()
                        .find(|s| s.end_sample.is_some())
                        .ok_or("Пока нет записанного отрезка для ответа.")?
                        .seq;
                    state.add_answer(seq, request)?;
                    Ok(())
                });
                if let Err(e) = result {
                    self.set_error(e);
                }
                self.schedule();
                self.changed();
            }
            CaptureEvent::Failed(error) => self.set_error(error),
            CaptureEvent::Stopped => {
                self.capture.lock().expect("capture mutex poisoned").take();
                if let Err(e) = self.update(target, |state| {
                    state.recording = false;
                    if let Some(last) = state.segments.last_mut() {
                        if last.end_sample.is_none() {
                            let path = self
                                .root
                                .join("sessions")
                                .join(&state.id)
                                .join(&last.audio_path);
                            if path.exists() {
                                let samples = audio::recover_wave(&path)?;
                                if samples > 0 {
                                    last.end_sample = Some(last.start_sample + samples);
                                } else {
                                    state.segments.pop();
                                }
                            } else {
                                state.segments.pop();
                            }
                        }
                    }
                    Ok(())
                }) {
                    self.set_error(e);
                }
                self.peak.store(0, Ordering::Relaxed);
                self.changed();
                self.schedule();
            }
        }
    }
    pub fn regenerate_answer(
        self: &Arc<Self>,
        session_id: &str,
        answer_id: u64,
    ) -> Result<(), String> {
        let target = self
            .sessions
            .lock()
            .map_err(|e| e.to_string())?
            .get(session_id)
            .cloned()
            .ok_or("Сессия не загружена.")?;
        let settings = self.settings();
        {
            let mut current = target.lock().map_err(|e| e.to_string())?;
            let mut state = current.clone();
            let answer = state.answer_mut(answer_id)?;
            if !matches!(answer.status, Status::Done | Status::Failed) {
                return Err("Ответ уже готовится. Дождитесь завершения.".into());
            }
            if let Some(text) = answer.text.take() {
                answer.history.push(session::AnswerRevision {
                    text,
                    model: answer.options.model.clone(),
                    reasoning_effort: answer.options.reasoning_effort.clone(),
                    speed: answer.options.speed.clone(),
                });
            }
            if let Some(action) = settings
                .actions
                .iter()
                .find(|a| a.id == answer.options.action.id)
            {
                answer.options.action = action.clone();
            }
            answer.options.model = settings.model;
            answer.options.reasoning_effort = settings.reasoning_effort;
            answer.options.speed = settings.speed;
            answer.status = Status::WaitingContext;
            answer.error = None;
            session::save(&self.root, &state)?;
            *current = state;
        }
        self.changed();
        self.schedule();
        Ok(())
    }
    pub fn retry(self: &Arc<Self>, seq: u64) -> Result<(), String> {
        let id = self
            .current
            .lock()
            .map_err(|e| e.to_string())?
            .clone()
            .ok_or("Нет сессии.")?;
        let target = self
            .sessions
            .lock()
            .map_err(|e| e.to_string())?
            .get(&id)
            .cloned()
            .ok_or("Нет сессии.")?;
        self.update(&target, |state| {
            let s = state.segment_mut(seq)?;
            if s.end_sample.is_none() {
                return Err("Этот отрезок ещё записывается.".into());
            }
            if s.transcription_status == Status::Failed {
                s.transcription_status = Status::Waiting;
                s.transcription_error = None;
            }
            Ok(())
        })?;
        self.clear_error();
        self.schedule();
        Ok(())
    }
    pub fn context_file(&self) -> Result<PathBuf, String> {
        let id = self
            .current
            .lock()
            .map_err(|e| e.to_string())?
            .clone()
            .ok_or("Нет сессии.")?;
        session::context_path(&self.root, &id)
    }
    pub fn busy(&self) -> bool {
        self.capture
            .lock()
            .expect("capture mutex poisoned")
            .is_some()
            || self.active_transcripts.load(Ordering::Relaxed) > 0
            || self.active_answers.load(Ordering::Relaxed) > 0
    }
    fn schedule(self: &Arc<Self>) {
        let _serial = self.scheduler.lock().expect("scheduler mutex poisoned");
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .expect("sessions mutex poisoned")
            .values()
            .cloned()
            .collect();
        let mut tasks = vec![];
        for target in sessions {
            let mut state = target.lock().expect("session mutex poisoned");
            let session_id = state.id.clone();
            for segment in &mut state.segments {
                if segment.end_sample.is_some()
                    && segment.transcription_status == Status::Waiting
                    && self.active_transcripts.load(Ordering::Relaxed) < 2
                {
                    segment.transcription_status = Status::Running;
                    self.active_transcripts.fetch_add(1, Ordering::Relaxed);
                    tasks.push(Task {
                        session: target.clone(),
                        id: segment.seq,
                        kind: Kind::Transcribe,
                        audio: self
                            .root
                            .join("sessions")
                            .join(&session_id)
                            .join(&segment.audio_path),
                        context: String::new(),
                        prompt: String::new(),
                        model: String::new(),
                        reasoning_effort: String::new(),
                        speed: String::new(),
                    });
                }
            }
            for index in 0..state.answers.len() {
                let answer = &state.answers[index];
                if !matches!(answer.status, Status::Waiting | Status::WaitingContext) {
                    continue;
                }
                let context = match state.through(answer.through_seq) {
                    Ok(value) => value,
                    Err(error) => {
                        *self.error.lock().expect("error mutex poisoned") = Some(error);
                        None
                    }
                };
                let answer = &mut state.answers[index];
                let Some(context) = context else {
                    answer.status = Status::WaitingContext;
                    continue;
                };
                if self.active_answers.load(Ordering::Relaxed) >= 2 {
                    continue;
                }
                answer.status = Status::Running;
                self.active_answers.fetch_add(1, Ordering::Relaxed);
                tasks.push(Task {
                    session: target.clone(),
                    id: answer.id,
                    kind: Kind::Answer,
                    audio: PathBuf::new(),
                    context,
                    prompt: answer.options.action.prompt.clone(),
                    model: answer.options.model.clone(),
                    reasoning_effort: answer.options.reasoning_effort.clone(),
                    speed: answer.options.speed.clone(),
                });
            }
        }
        for task in tasks {
            let service = self.clone();
            thread::spawn(move || service.run_task(task));
        }
        self.changed();
    }
    fn run_task(self: Arc<Self>, task: Task) {
        let result = match task.kind {
            Kind::Transcribe => self.backend.transcribe(&task.audio),
            Kind::Answer => self.backend.answer(
                &task.context,
                &task.prompt,
                &task.model,
                &task.reasoning_effort,
                &task.speed,
            ),
        };
        let update = self.update(&task.session, |state| {
            match task.kind {
                Kind::Transcribe => {
                    let segment = state.segment_mut(task.id)?;
                    match result {
                        Ok(text) => {
                            segment.text = Some(text);
                            segment.transcription_status = Status::Done;
                            segment.transcription_error = None;
                        }
                        Err(error) => {
                            segment.transcription_status = Status::Failed;
                            segment.transcription_error = Some(error);
                        }
                    }
                }
                Kind::Answer => {
                    let answer = state.answer_mut(task.id)?;
                    match result {
                        Ok(text) => {
                            answer.text = Some(text);
                            answer.status = Status::Done;
                            answer.error = None;
                        }
                        Err(error) => {
                            answer.status = Status::Failed;
                            answer.error = Some(error);
                        }
                    }
                }
            }
            Ok(())
        });
        match task.kind {
            Kind::Transcribe => {
                self.active_transcripts.fetch_sub(1, Ordering::Relaxed);
            }
            Kind::Answer => {
                self.active_answers.fetch_sub(1, Ordering::Relaxed);
            }
        }
        if let Err(error) = update {
            self.set_error(error);
        }
        self.changed();
        self.schedule();
        self.changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::Path,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };
    type AnswerCall = (String, String, String, String, String);
    struct Fake {
        release_transcript: AtomicBool,
        release_answers: AtomicBool,
        fail_answer: AtomicBool,
        transcriptions: AtomicUsize,
        calls: Mutex<Vec<AnswerCall>>,
    }
    impl Backend for Fake {
        fn transcribe(&self, audio: &Path) -> Result<String, String> {
            self.transcriptions.fetch_add(1, Ordering::Relaxed);
            let first = audio.file_name().unwrap() == "000001.wav";
            if first {
                while !self.release_transcript.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(if first { "first" } else { "second" }.into())
        }
        fn answer(
            &self,
            context: &str,
            prompt: &str,
            model: &str,
            effort: &str,
            speed: &str,
        ) -> Result<String, String> {
            self.calls.lock().unwrap().push((
                context.into(),
                prompt.into(),
                model.into(),
                effort.into(),
                speed.into(),
            ));
            while !self.release_answers.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            if self.fail_answer.load(Ordering::Acquire) {
                return Err("test generation failure".into());
            }
            Ok(format!("response/{model}/{effort}/{prompt}"))
        }
    }
    fn backend() -> Arc<Fake> {
        Arc::new(Fake {
            release_transcript: AtomicBool::new(true),
            release_answers: AtomicBool::new(true),
            fail_answer: AtomicBool::new(false),
            transcriptions: AtomicUsize::new(0),
            calls: Mutex::new(vec![]),
        })
    }
    fn wait_for(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "background task timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn fixture(service: &Service, ready: bool) -> SharedSession {
        let mut state = Session::new(&Settings::default());
        state.recording = true;
        let texts = if ready {
            vec!["first", "second", "third"]
        } else {
            vec!["first", "second"]
        };
        for (i, text) in texts.iter().enumerate() {
            let mut segment = Segment::recording(i as u64 + 1, i as u64 * 24000);
            segment.end_sample = Some((i as u64 + 1) * 24000);
            if ready {
                segment.text = Some((*text).into());
                segment.transcription_status = Status::Done;
            }
            state.segments.push(segment);
        }
        state.segments.push(Segment::recording(
            texts.len() as u64 + 1,
            texts.len() as u64 * 24000,
        ));
        if ready {
            state
                .add_answer(2, service.settings().answer_options("brief").unwrap())
                .unwrap();
            state.answers[0].text = Some("original low answer".into());
            state.answers[0].status = Status::Done;
        }
        session::save(&service.root, &state).unwrap();
        let id = state.id.clone();
        let target = Arc::new(Mutex::new(state));
        service
            .sessions
            .lock()
            .unwrap()
            .insert(id.clone(), target.clone());
        *service.current.lock().unwrap() = Some(id);
        target
    }
    #[test]
    fn ordinary_segments_only_transcribe_and_write_plain_context_in_order() {
        let root = tempfile::tempdir().unwrap();
        let backend = backend();
        backend.release_transcript.store(false, Ordering::Release);
        let service =
            Service::new(root.path().into(), backend.clone(), Arc::new(|_, _| {})).unwrap();
        let target = fixture(&service, false);
        service.schedule();
        wait_for(|| target.lock().unwrap().segments[1].transcription_status == Status::Done);
        let path = service.context_file().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        backend.release_transcript.store(true, Ordering::Release);
        wait_for(|| service.active_transcripts.load(Ordering::Relaxed) == 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\n\nsecond");
        assert!(backend.calls.lock().unwrap().is_empty());
        assert!(target.lock().unwrap().answers.is_empty());
        assert!(target.lock().unwrap().recording);
        assert!(!path.with_file_name("answers.txt").exists());
        assert!(!path.with_file_name("session.json").exists());
    }
    #[test]
    fn two_actions_on_the_same_boundary_keep_separate_prompts_and_wait_for_context() {
        let root = tempfile::tempdir().unwrap();
        let backend = backend();
        backend.release_transcript.store(false, Ordering::Release);
        backend.release_answers.store(false, Ordering::Release);
        let service =
            Service::new(root.path().into(), backend.clone(), Arc::new(|_, _| {})).unwrap();
        let target = fixture(&service, false);
        let settings = service.settings();
        for id in ["answer", "brief"] {
            service.capture_event(
                &target,
                CaptureEvent::Empty {
                    request: settings.answer_options(id).unwrap(),
                },
            );
        }
        wait_for(|| target.lock().unwrap().segments[1].transcription_status == Status::Done);
        assert!(backend.calls.lock().unwrap().is_empty());
        let mut changed = settings.clone();
        changed.actions.clear();
        service.save_settings(changed).unwrap();
        service
            .save_model_settings("gpt-6-astra".into(), "high".into(), "fast".into())
            .unwrap();
        backend.release_transcript.store(true, Ordering::Release);
        wait_for(|| backend.calls.lock().unwrap().len() == 2);
        {
            let calls = backend.calls.lock().unwrap();
            for (context, prompt, model, effort, speed) in calls.iter() {
                assert_eq!(context, "first\n\nsecond");
                assert!(settings.actions[..2].iter().any(|a| a.prompt == *prompt));
                assert_eq!(
                    (model.as_str(), effort.as_str(), speed.as_str()),
                    ("gpt-6-luna", "low", "standard")
                );
            }
            assert_ne!(calls[0].1, calls[1].1);
        }
        assert_eq!(service.active_answers.load(Ordering::Relaxed), 2);
        backend.release_answers.store(true, Ordering::Release);
        wait_for(|| service.active_answers.load(Ordering::Relaxed) == 0);
        let state = target.lock().unwrap();
        assert_eq!(state.answers.len(), 2);
        assert!(state
            .answers
            .iter()
            .all(|a| a.status == Status::Done && a.through_seq == 2));
        assert_eq!(state.answers[1].options.action.id, "brief");
        assert!(state.recording && state.segments.last().unwrap().end_sample.is_none());
        assert_eq!(
            fs::read_to_string(service.context_file().unwrap()).unwrap(),
            "first\n\nsecond"
        );
    }
    #[test]
    fn regeneration_uses_the_same_action_current_options_and_original_context() {
        let root = tempfile::tempdir().unwrap();
        let backend = backend();
        backend.release_answers.store(false, Ordering::Release);
        let service =
            Service::new(root.path().into(), backend.clone(), Arc::new(|_, _| {})).unwrap();
        let target = fixture(&service, true);
        let id = target.lock().unwrap().id.clone();
        service
            .save_model_settings("gpt-6-astra".into(), "high".into(), "fast".into())
            .unwrap();
        let mut settings = service.settings();
        settings.actions[1].prompt = "updated brief prompt".into();
        settings.actions[0].prompt = "unrelated prompt".into();
        settings.speed = "standard".into();
        service.save_settings(settings).unwrap();
        assert_eq!(service.settings().speed, "fast");
        service.regenerate_answer(&id, 1).unwrap();
        wait_for(|| backend.calls.lock().unwrap().len() == 1);
        assert!(service.regenerate_answer(&id, 1).is_err());
        assert!(service.regenerate_answer(&id, 99).is_err());
        assert!(service.regenerate_answer("not-loaded", 1).is_err());
        assert_eq!(
            backend.calls.lock().unwrap()[0],
            (
                "first\n\nsecond".into(),
                "updated brief prompt".into(),
                "gpt-6-astra".into(),
                "high".into(),
                "fast".into()
            )
        );
        {
            let state = target.lock().unwrap();
            assert!(state.recording);
            assert_eq!(state.segments.len(), 4);
            assert_eq!(state.segments[3].start_sample, 72000);
            assert!(state.segments[3].end_sample.is_none());
            assert_eq!(state.answers[0].history[0].text, "original low answer");
            assert_eq!(state.answers[0].history[0].speed, "standard");
        }
        service
            .save_model_settings("gpt-6-luna".into(), "medium".into(), "standard".into())
            .unwrap();
        backend.release_answers.store(true, Ordering::Release);
        wait_for(|| service.active_answers.load(Ordering::Relaxed) == 0);
        let state = target.lock().unwrap();
        assert_eq!(
            state.answers[0].text.as_deref(),
            Some("response/gpt-6-astra/high/updated brief prompt")
        );
        let path = service.context_file().unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "first\n\nsecond\n\nthird"
        );
        assert_eq!(state.answers[0].options.speed, "fast");
        let answers = fs::read_to_string(path.with_file_name("answers.txt")).unwrap();
        assert!(answers.contains("/ fast") && answers.contains("/ standard"));
        assert!(
            answers.contains("original low answer")
                && answers.contains("response/gpt-6-astra/high/updated brief prompt")
        );
        assert_eq!(backend.transcriptions.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn deleting_an_action_and_failed_regeneration_keep_the_previous_result() {
        let root = tempfile::tempdir().unwrap();
        let backend = backend();
        backend.fail_answer.store(true, Ordering::Release);
        let service =
            Service::new(root.path().into(), backend.clone(), Arc::new(|_, _| {})).unwrap();
        let target = fixture(&service, true);
        target.lock().unwrap().recording = false;
        let id = target.lock().unwrap().id.clone();
        let original_prompt = service.settings().actions[1].prompt.clone();
        let mut settings = service.settings();
        settings.actions.remove(1);
        service.save_settings(settings).unwrap();
        service.regenerate_answer(&id, 1).unwrap();
        wait_for(|| service.active_answers.load(Ordering::Relaxed) == 0);
        assert_eq!(target.lock().unwrap().answers[0].status, Status::Failed);
        assert_eq!(
            target.lock().unwrap().answers[0].history[0].text,
            "original low answer"
        );
        assert_eq!(backend.calls.lock().unwrap()[0].1, original_prompt);
        let file = service
            .context_file()
            .unwrap()
            .with_file_name("answers.txt");
        assert!(fs::read_to_string(file)
            .unwrap()
            .contains("original low answer"));
        backend.fail_answer.store(false, Ordering::Release);
        service
            .save_model_settings("gpt-6-luna".into(), "medium".into(), "standard".into())
            .unwrap();
        service.regenerate_answer(&id, 1).unwrap();
        wait_for(|| service.active_answers.load(Ordering::Relaxed) == 0);
        let state = target.lock().unwrap();
        assert_eq!(state.answers[0].status, Status::Done);
        assert_eq!(state.answers[0].history.len(), 1);
        assert!(!state.recording);
        assert_eq!(backend.calls.lock().unwrap().len(), 2);
    }
    #[test]
    fn failed_save_keeps_the_ready_answer_and_does_not_call_the_model() {
        let root = tempfile::tempdir().unwrap();
        let backend = backend();
        let service =
            Service::new(root.path().into(), backend.clone(), Arc::new(|_, _| {})).unwrap();
        let target = fixture(&service, true);
        let id = target.lock().unwrap().id.clone();
        let path = service.context_file().unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(path).unwrap();
        assert!(service.regenerate_answer(&id, 1).is_err());
        let state = target.lock().unwrap();
        assert_eq!(
            state.answers[0].text.as_deref(),
            Some("original low answer")
        );
        assert_eq!(state.answers[0].status, Status::Done);
        assert!(state.answers[0].history.is_empty());
        assert!(backend.calls.lock().unwrap().is_empty());
    }
    #[test]
    fn restart_keeps_files_and_action_settings_without_restoring_the_queue() {
        let root = tempfile::tempdir().unwrap();
        let backend = backend();
        let service =
            Service::new(root.path().into(), backend.clone(), Arc::new(|_, _| {})).unwrap();
        fixture(&service, true);
        let context = service.context_file().unwrap();
        let before = fs::read(&context).unwrap();
        service
            .save_model_settings("gpt-6-luna".into(), "low".into(), "fast".into())
            .unwrap();
        let mut settings = service.settings();
        settings.actions[1].name = "Моя кнопка".into();
        service.save_settings(settings).unwrap();
        drop(service);
        let restarted =
            Service::new(root.path().into(), backend.clone(), Arc::new(|_, _| {})).unwrap();
        assert!(restarted.snapshot()["session"].is_null());
        assert_eq!(restarted.snapshot()["sessions"], json!([]));
        assert_eq!(restarted.settings().actions[1].name, "Моя кнопка");
        assert_eq!(restarted.settings().speed, "fast");
        assert_eq!(fs::read(context).unwrap(), before);
        assert!(!restarted.busy());
    }
}
