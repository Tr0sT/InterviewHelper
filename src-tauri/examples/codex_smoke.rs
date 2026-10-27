use interview_helper::{
    codex::{Backend, Codex},
    session::Settings,
};
fn main() -> Result<(), String> {
    let codex = Codex;
    let settings = Settings::default();
    println!("{}", codex.check()?);
    if let Some(path) = std::env::args().nth(1) {
        let started = std::time::Instant::now();
        let text = codex.transcribe(std::path::Path::new(&path))?;
        println!(
            "transcription ({:.2}s): {text}",
            started.elapsed().as_secs_f64()
        );
        assert!(!text.trim().is_empty());
    }
    let context="[Отрезок #001]\nВопрос: Почему вы начали с MVP? Ответ: Сначала проверяем основной сценарий.\n[Отрезок #002]\nВопрос: Как решение выдержит рост нагрузки?";
    let options = settings.answer_options("answer")?;
    let answer = codex.answer(
        context,
        &options.action.prompt,
        &options.model,
        &options.reasoning_effort,
        &options.speed,
    )?;
    assert!(!answer.trim().is_empty());
    println!("{answer}");
    Ok(())
}
