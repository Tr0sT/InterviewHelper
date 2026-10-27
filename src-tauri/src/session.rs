use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SAMPLE_RATE: u32 = 24_000;
pub const DEFAULT_MODEL: &str = "gpt-6-luna";
pub const DEFAULT_REASONING_EFFORT: &str = "low";
pub const ANSWER_PROMPT: &str = "Подготовь короткий ответ от моего лица на последний актуальный вопрос в разговоре. Учти весь предоставленный контекст. Дай готовый текст для чтения вслух, без служебных пояснений. Не выдумывай мой опыт, обязательства и неизвестные факты. Если вопрос или необходимые данные неясны, предложи уточнение.";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptAction {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub shortcut: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AnswerOptions {
    pub action: PromptAction,
    pub model: String,
    pub reasoning_effort: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub microphone: String,
    pub output: String,
    pub actions: Vec<PromptAction>,
    pub model: String,
    pub reasoning_effort: String,
    pub cut_shortcut: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            microphone: String::new(),
            output: String::new(),
            actions: vec![
                PromptAction { id: "answer".into(), name: "Ответь".into(), prompt: ANSWER_PROMPT.into(), shortcut: "Ctrl+Alt+Enter".into() },
                PromptAction { id: "brief".into(), name: "Кратко".into(), prompt: "Ответь от моего лица на последний актуальный вопрос в разговоре в одном-двух предложениях. Дай готовый текст для чтения вслух. Не выдумывай факты и мой опыт.".into(), shortcut: "Ctrl+Alt+K".into() },
                PromptAction { id: "clarify".into(), name: "Уточни".into(), prompt: "Предложи один короткий уточняющий вопрос от моего лица, который поможет ответить на последний вопрос собеседника. Учти историю разговора. Дай только готовую реплику для чтения вслух.".into(), shortcut: "Ctrl+Alt+U".into() },
            ],
            model: DEFAULT_MODEL.into(),
            reasoning_effort: DEFAULT_REASONING_EFFORT.into(),
            cut_shortcut: "Ctrl+Alt+Space".into(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        let cut = crate::hotkeys::parse(&self.cut_shortcut)?;
        let mut shortcuts = HashSet::from([(cut.mask, cut.key)]);
        let mut ids = HashSet::new();
        for action in &self.actions {
            if action.id.is_empty()
                || action.id.len() > 64
                || !action
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !ids.insert(&action.id)
            {
                return Err("Некорректный или повторяющийся ID кнопки.".into());
            }
            if action.name.trim().is_empty()
                || action.name.len() > 256
                || action.name.chars().any(char::is_control)
            {
                return Err("Укажите название кнопки длиной до 256 байт.".into());
            }
            if action.prompt.trim().is_empty() || action.prompt.len() > 32_000 {
                return Err(format!(
                    "Для кнопки «{}» нужен промпт длиной до 32 000 байт.",
                    action.name
                ));
            }
            if !action.shortcut.trim().is_empty() {
                let shortcut = crate::hotkeys::parse(&action.shortcut)?;
                if !shortcuts.insert((shortcut.mask, shortcut.key)) {
                    return Err(format!(
                        "Горячая клавиша кнопки «{}» уже используется другим действием.",
                        action.name
                    ));
                }
            }
        }
        if self.model.trim().is_empty()
            || self.model.len() > 128
            || self.model.chars().any(char::is_whitespace)
        {
            return Err("Выберите модель Codex.".into());
        }
        if ![
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ]
        .contains(&self.reasoning_effort.as_str())
        {
            return Err("Выберите поддерживаемый уровень reasoning.".into());
        }
        Ok(())
    }
    pub fn answer_options(&self, action_id: &str) -> Result<AnswerOptions, String> {
        let action = self
            .actions
            .iter()
            .find(|action| action.id == action_id)
            .ok_or("Кнопка больше не существует. Проверьте настройки.")?;
        Ok(AnswerOptions {
            action: action.clone(),
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Waiting,
    Running,
    WaitingContext,
    Done,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct AnswerRevision {
    pub text: String,
    pub model: String,
    pub reasoning_effort: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Answer {
    pub id: u64,
    pub through_seq: u64,
    pub options: AnswerOptions,
    pub status: Status,
    pub text: Option<String>,
    pub history: Vec<AnswerRevision>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Segment {
    pub seq: u64,
    pub start_sample: u64,
    pub end_sample: Option<u64>,
    pub audio_path: String,
    pub transcription_status: Status,
    pub text: Option<String>,
    pub transcription_error: Option<String>,
}
impl Segment {
    pub fn recording(seq: u64, start_sample: u64) -> Self {
        Self {
            seq,
            start_sample,
            end_sample: None,
            audio_path: format!("audio/{seq:06}.wav"),
            transcription_status: Status::Waiting,
            text: None,
            transcription_error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Session {
    pub id: String,
    pub created_ms: u64,
    pub recording: bool,
    pub microphone: String,
    pub output: String,
    pub segments: Vec<Segment>,
    pub answers: Vec<Answer>,
}
impl Session {
    pub fn new(settings: &Settings) -> Self {
        let created_ms = now_ms();
        Self {
            id: format!("{created_ms}-{}", std::process::id()),
            created_ms,
            recording: false,
            microphone: settings.microphone.clone(),
            output: settings.output.clone(),
            segments: vec![],
            answers: vec![],
        }
    }
    pub fn through(&self, seq: u64) -> Result<Option<String>, String> {
        if seq == 0 || !self.segments.iter().any(|s| s.seq == seq) {
            return Err("Неизвестная граница контекста.".into());
        }
        if self
            .segments
            .iter()
            .any(|s| s.seq <= seq && s.transcription_status != Status::Done)
        {
            return Ok(None);
        }
        self.transcript(seq).map(Some)
    }
    fn transcript(&self, through: u64) -> Result<String, String> {
        let mut parts = vec![];
        for segment in self
            .segments
            .iter()
            .filter(|s| s.seq <= through && s.transcription_status == Status::Done)
        {
            let text = segment
                .text
                .as_deref()
                .ok_or("У готового отрезка нет расшифровки.")?;
            if !text.is_empty() {
                parts.push(text);
            }
        }
        Ok(parts.join("\n\n"))
    }
    pub fn add_answer(&mut self, through_seq: u64, options: AnswerOptions) -> Result<u64, String> {
        if !self
            .segments
            .iter()
            .any(|s| s.seq == through_seq && s.end_sample.is_some())
        {
            return Err("Пока нет записанного отрезка для ответа.".into());
        }
        let id = self.answers.last().map_or(1, |a| a.id + 1);
        self.answers.push(Answer {
            id,
            through_seq,
            options,
            status: Status::WaitingContext,
            text: None,
            history: vec![],
            error: None,
        });
        Ok(id)
    }
    pub fn answer_mut(&mut self, id: u64) -> Result<&mut Answer, String> {
        self.answers
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| "Запрос ответа не найден.".into())
    }
    pub fn segment_mut(&mut self, seq: u64) -> Result<&mut Segment, String> {
        self.segments
            .iter_mut()
            .find(|s| s.seq == seq)
            .ok_or_else(|| format!("Отрезок #{seq} не найден."))
    }
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err("Некорректный ID сессии.".into());
        }
        let mut end = 0;
        for (index, s) in self.segments.iter().enumerate() {
            if s.seq != index as u64 + 1
                || s.start_sample != end
                || s.audio_path != format!("audio/{:06}.wav", s.seq)
                || s.end_sample.is_some_and(|e| e < s.start_sample)
                || (s.end_sample.is_none() && index + 1 != self.segments.len())
            {
                return Err("Нарушена последовательность отрезков сессии.".into());
            }
            end = s.end_sample.unwrap_or(s.start_sample);
        }
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock precedes Unix epoch")
        .as_millis() as u64
}
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() < 80 && id.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}
pub fn private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("Не удалось создать {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("Путь файла без каталога.")?;
    private_dir(parent)?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| format!("Ошибка сохранения: {e}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("Ошибка записи: {e}"))?;
    fs::rename(&temp, path).map_err(|e| format!("Не удалось обновить файл: {e}"))?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("Не удалось синхронизировать каталог: {e}"))
}
pub fn context_path(root: &Path, id: &str) -> Result<PathBuf, String> {
    if !valid_id(id) {
        return Err("Некорректный ID сессии.".into());
    }
    Ok(root.join("sessions").join(id).join("context.txt"))
}
pub fn save(root: &Path, session: &Session) -> Result<(), String> {
    session.validate()?;
    let path = context_path(root, &session.id)?;
    atomic_write(&path, session.transcript(u64::MAX)?.as_bytes())?;
    let mut answers = String::new();
    for answer in &session.answers {
        for (text, model, effort) in answer
            .history
            .iter()
            .map(|a| (&a.text, &a.model, &a.reasoning_effort))
            .chain(answer.text.as_ref().map(|text| {
                (
                    text,
                    &answer.options.model,
                    &answer.options.reasoning_effort,
                )
            }))
        {
            answers.push_str(&format!(
                "{} · после отрезка #{:03} · {model} / {effort}\n{text}\n\n",
                answer.options.action.name, answer.through_seq
            ));
        }
    }
    if !answers.is_empty() {
        atomic_write(&path.with_file_name("answers.txt"), answers.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Session {
        let mut s = Session::new(&Settings::default());
        for i in 0..3 {
            let mut segment = Segment::recording(i + 1, i * 24000);
            segment.end_sample = Some((i + 1) * 24000);
            s.segments.push(segment);
        }
        s
    }
    #[test]
    fn actions_validate_ids_prompts_and_normalized_shortcuts() {
        let mut settings = Settings::default();
        settings.validate().unwrap();
        assert_eq!(
            (&*settings.model, &*settings.reasoning_effort),
            ("gpt-6-luna", "low")
        );
        settings.actions[1].shortcut = "Alt+Ctrl+Return".into();
        assert!(settings.validate().is_err());
        settings.actions[1].shortcut = "Alt+Ctrl+Space".into();
        assert!(settings.validate().is_err());
        settings.actions[1].shortcut.clear();
        settings.validate().unwrap();
        settings.actions[1].id = settings.actions[0].id.clone();
        assert!(settings.validate().is_err());
        settings.actions[1].id = "brief;echo nope".into();
        assert!(settings.validate().is_err());
        settings.actions[1].id = "brief".into();
        settings.actions[1].prompt = " ".into();
        assert!(settings.validate().is_err());
        settings.actions.clear();
        settings.validate().unwrap();
        assert!(settings.answer_options("answer").is_err());
    }
    #[test]
    fn each_action_keeps_its_submission_options_and_its_own_id() {
        let mut settings = Settings::default();
        let mut session = fixture();
        let first = session
            .add_answer(2, settings.answer_options("answer").unwrap())
            .unwrap();
        let second = session
            .add_answer(2, settings.answer_options("brief").unwrap())
            .unwrap();
        assert_ne!(first, second);
        settings.actions[0].prompt = "changed later".into();
        settings.model = "gpt-6-astra".into();
        assert_eq!(session.answers[0].options.action.prompt, ANSWER_PROMPT);
        assert_eq!(session.answers[0].options.model, "gpt-6-luna");
        assert_eq!(session.answers[1].options.action.id, "brief");
        assert!(session
            .add_answer(99, settings.answer_options("answer").unwrap())
            .is_err());
    }
    #[test]
    fn out_of_order_transcripts_wait_for_the_complete_prefix() {
        let mut s = fixture();
        s.segments[1].text = Some("second".into());
        s.segments[1].transcription_status = Status::Done;
        assert_eq!(s.through(2).unwrap(), None);
        s.segments[0].text = Some("first".into());
        s.segments[0].transcription_status = Status::Done;
        assert_eq!(s.through(2).unwrap().unwrap(), "first\n\nsecond");
    }
    #[test]
    fn text_files_keep_recording_order_and_separate_answers() {
        let mut s = fixture();
        let dir = tempfile::tempdir().unwrap();
        s.segments[1].text = Some("Второй отрезок.\nЕщё строка.".into());
        s.segments[1].transcription_status = Status::Done;
        save(dir.path(), &s).unwrap();
        let path = context_path(dir.path(), &s.id).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "Второй отрезок.\nЕщё строка."
        );
        s.segments[0].text = Some("Первый отрезок.".into());
        s.segments[0].transcription_status = Status::Done;
        s.add_answer(2, Settings::default().answer_options("answer").unwrap())
            .unwrap();
        s.answers[0].text = Some("Подсказка модели.".into());
        s.answers[0].status = Status::Done;
        save(dir.path(), &s).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "Первый отрезок.\n\nВторой отрезок.\nЕщё строка."
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            s.through(2).unwrap().unwrap()
        );
        assert_eq!(
            fs::read_to_string(path.with_file_name("answers.txt")).unwrap(),
            "Ответь · после отрезка #002 · gpt-6-luna / low\nПодсказка модели.\n\n"
        );
        assert!(!path.with_file_name("session.json").exists());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(context_path(dir.path(), "../escape").is_err());
        s.segments[0].audio_path = "/tmp/other.wav".into();
        assert!(save(dir.path(), &s).is_err());
    }
}
