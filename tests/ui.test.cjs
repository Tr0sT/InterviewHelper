const { test } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('@playwright/test');

test('dynamic actions, independent answers, settings and continuous recording', async () => {
  const server = http.createServer((request, response) => {
    const routes = { '/': ['index.html', 'text/html'], '/app.js': ['app.js', 'text/javascript'], '/style.css': ['style.css', 'text/css'] };
    const route = routes[request.url];
    if (!route) { response.writeHead(404).end(); return; }
    response.setHeader('Content-Type', `${route[1]}; charset=utf-8`);
    response.end(fs.readFileSync(path.join(__dirname, '..', 'ui', route[0])));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
    const page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.addInitScript(() => {
      window.calls = [];
      const listeners = {};
      const state = { settings: { microphone: 'mic', output: 'out.monitor', actions: [
        { id: 'answer', name: 'Ответь', prompt: 'Ответь на вопрос', shortcut: 'Ctrl+Alt+Enter' },
        { id: 'brief', name: 'Кратко', prompt: 'Дай краткий ответ', shortcut: 'Ctrl+Alt+K' },
        { id: 'clarify', name: 'Уточни', prompt: 'Задай уточняющий вопрос', shortcut: 'Ctrl+Alt+U' },
      ], model: 'gpt-6-luna', reasoning_effort: 'low', speed: 'standard', cut_shortcut: 'Ctrl+Alt+Space' }, session: null, sessions: [], recording: false, samples: 0, peak: 0, error: null, hotkey_error: null };
      window.pushState = (changes) => { Object.assign(state, changes); for (const listener of listeners.state || []) listener({ payload: structuredClone(state) }); };
      window.finishAnswer = (id, text, error = null) => { const answer = state.session.answers.find((a) => a.id === id); answer.text = error ? null : text; answer.error = error; answer.status = error ? 'failed' : 'done'; window.pushState({}); };
      window.optionsFor = (id) => ({ action: structuredClone(state.settings.actions.find((a) => a.id === id)), model: state.settings.model, reasoning_effort: state.settings.reasoning_effort, speed: state.settings.speed });
      window.manyActions = () => window.pushState({ settings: { ...state.settings, actions: Array.from({ length: 12 }, (_, i) => ({ id: `custom-${i}`, name: `Действие ${i + 1} с длинным названием`, prompt: 'Тест', shortcut: '' })) } });
      window.__TAURI__ = {
        event: { listen: async (name, callback) => { (listeners[name] ||= []).push(callback); return () => {}; } },
        core: { invoke: async (command, params) => {
          window.calls.push({ command, params });
          if (command === 'snapshot') return structuredClone(state);
          if (command === 'audio_devices') return { microphones: [{ name: 'mic', description: 'Test microphone' }], outputs: [{ name: 'out.monitor', description: 'Test output' }], default_microphone: 'mic', default_output: 'out.monitor' };
          if (command === 'check_codex') return { signed_in: true, luna: true, transcription_login: true, models: [
            { id: 'gpt-6-luna', label: 'GPT-6-Luna', default_reasoning_effort: 'medium', reasoning_efforts: ['low', 'medium', 'high'] },
            { id: 'gpt-6-astra', label: 'GPT-6-Astra', default_reasoning_effort: 'medium', reasoning_efforts: ['low', 'medium', 'high', 'ultra'] },
          ] };
          if (command === 'start_recording') window.pushState({ recording: true, sessions: ['1793000000000-1'], session: { id: '1793000000000-1', created_ms: 1793000000000, microphone: 'mic', output: 'out.monitor', segments: [{ seq: 1, start_sample: 0, end_sample: null, text: null }], answers: [] } });
          if (command === 'stop_recording') { state.session.segments = state.session.segments.filter((segment) => segment.end_sample !== null); window.pushState({ recording: false }); }
          if (command === 'save_settings') {
            if (window.rejectSettings) throw new Error('Горячая клавиша уже используется.');
            window.pushState({ settings: { ...state.settings, ...params.settings } });
          }
          if (command === 'save_model_settings') window.pushState({ settings: { ...state.settings, model: params.model, reasoning_effort: params.reasoningEffort, speed: params.speed } });
          if (command === 'regenerate_answer') {
            const answer = state.session.answers.find((item) => item.id === params.answerId);
            if (answer.text) answer.history.push({ text: answer.text, model: answer.options.model, reasoning_effort: answer.options.reasoning_effort, speed: answer.options.speed });
            Object.assign(answer, { text: null, status: 'running', error: null, options: window.optionsFor(answer.options.action.id) });
            window.pushState({});
          }
          return null;
        } },
      };
    });
    await page.goto(`http://127.0.0.1:${server.address().port}/`);
    assert.equal(await page.locator('#settings-dialog').evaluate((el) => el.open), false);
    assert.equal(await page.locator('#prompt-actions button').count(), 3);
    assert.equal(await page.locator('#regenerate-button').isEnabled(), false);
    assert.equal(await page.getByLabel('Скорость ответа').inputValue(), 'standard');
    assert.equal(await page.getByLabel('Reasoning', { exact: true }).inputValue(), 'low');
    await page.getByRole('button', { name: '● Начать', exact: true }).click();
    await page.getByRole('button', { name: 'Файл контекста', exact: false }).click();
    assert.equal(await page.evaluate(() => window.calls.at(-1).command), 'open_context_file');
    await page.getByRole('button', { name: 'Распознать отрезок', exact: false }).click();
    await page.locator('[data-action-id="answer"]').click();
    await page.locator('[data-action-id="brief"]').click();
    assert.deepEqual(await page.evaluate(() => window.calls.filter((c) => c.command === 'submit_segment').map((c) => c.params.actionId)), [null, 'answer', 'brief']);
    await page.evaluate(() => window.pushState({ samples: 72000, session: { id: '1793000000000-1', created_ms: 1793000000000, microphone: 'mic', output: 'out.monitor', segments: [
      { seq: 1, start_sample: 0, end_sample: 24000, transcription_status: 'done', text: '<img src=x onerror="window.injected=true">' },
      { seq: 2, start_sample: 24000, end_sample: 48000, transcription_status: 'done', text: 'Второй отрезок' },
      { seq: 3, start_sample: 48000, end_sample: 72000, text: 'Третий отрезок только в контекст', transcription_status: 'done' },
      { seq: 4, start_sample: 72000, end_sample: null, text: null },
    ], answers: [
      { id: 1, through_seq: 1, options: window.optionsFor('answer'), status: 'done', text: 'Старый ответ', history: [] },
      { id: 2, through_seq: 2, options: window.optionsFor('brief'), status: 'running', text: null, history: [] },
    ] } }));
    assert.equal(await page.locator('#record-label').textContent(), 'Идёт запись');
    assert.equal(await page.locator('#answer-content').textContent(), 'Luna готовит ответ…');
    assert.equal(await page.locator('#answer-boundary').textContent(), 'Кратко · до #002');
    assert.equal(await page.locator('#copy-answer').isEnabled(), false);
    assert.equal(await page.locator('#transcripts img').count(), 0);
    assert.equal(await page.evaluate(() => window.injected), undefined);
    assert((await page.locator('#transcripts').textContent()).includes('<img src=x'));
    await page.getByLabel('Показать ответ', { exact: true }).selectOption('1');
    await page.evaluate(() => window.finishAnswer(2, 'Краткий ответ'));
    assert.equal(await page.locator('#answer-content').textContent(), 'Старый ответ');
    await page.getByLabel('Показать ответ', { exact: true }).selectOption('2');
    assert.equal(await page.locator('#answer-content').textContent(), 'Краткий ответ');
    await page.getByLabel('Модель для ответа').selectOption('gpt-6-astra');
    await page.getByLabel('Reasoning', { exact: true }).selectOption('ultra');
    await page.getByLabel('Модель для ответа').selectOption('gpt-6-luna');
    assert.equal(await page.getByLabel('Reasoning', { exact: true }).inputValue(), 'low');
    assert.equal(await page.locator('#reasoning-select option[value="ultra"]').count(), 0);
    await page.getByLabel('Модель для ответа').selectOption('gpt-6-astra');
    await page.getByLabel('Reasoning', { exact: true }).selectOption('high');
    await page.waitForFunction(() => !document.getElementById('model-select').disabled);
    await page.getByLabel('Скорость ответа').selectOption('fast');
    await page.waitForFunction(() => !document.getElementById('speed-select').disabled);
    assert.equal(await page.evaluate(() => window.calls.filter((c) => c.command === 'save_model_settings').at(-1).params.speed), 'fast');
    assert.equal(await page.evaluate(() => window.calls.filter((c) => c.command === 'regenerate_answer').length), 0);
    await page.getByRole('button', { name: 'Настройки', exact: false }).click();
    assert.equal(await page.getByLabel('Микрофон', { exact: true }).locator('option:checked').textContent(), 'Test microphone');
    const brief = page.locator('.action-editor').nth(1);
    await brief.getByLabel('Название кнопки', { exact: true }).fill('Тезисы');
    await brief.getByLabel('Промпт', { exact: true }).fill('Новый промпт тезисов');
    await brief.getByLabel('Горячая клавиша', { exact: true }).fill('Ctrl+Alt+B');
    await page.locator('.action-editor').nth(2).getByRole('button', { name: 'Удалить кнопку' }).click();
    await page.getByRole('button', { name: '+ Добавить кнопку', exact: true }).click();
    const added = page.locator('.action-editor').last();
    await added.getByLabel('Название кнопки', { exact: true }).fill('План');
    await added.getByLabel('Промпт', { exact: true }).fill('Предложи план');
    await page.evaluate(() => { window.rejectSettings = true; });
    await page.getByRole('button', { name: 'Сохранить настройки' }).click();
    assert.equal(await page.locator('#settings-dialog').evaluate((el) => el.open), true);
    assert.match(await page.locator('#settings-message').textContent(), /Горячая клавиша уже используется\./);
    await page.evaluate(() => { window.rejectSettings = false; });
    await page.getByRole('button', { name: 'Сохранить настройки' }).click();
    assert.equal(await page.locator('#settings-dialog').evaluate((el) => el.open), false);
    assert.equal(await page.locator('#prompt-preview').textContent(), 'Ответь · Тезисы · План');
    assert.equal(await page.locator('#prompt-actions button').count(), 3);
    assert.equal(await page.locator('#prompt-actions').getByRole('button', { name: 'Уточни', exact: false }).count(), 0);
    assert.equal(await page.locator('[data-action-id="brief"] kbd').textContent(), 'Ctrl + Alt + B');
    assert.equal(await page.getByLabel('Reasoning', { exact: true }).inputValue(), 'high');
    assert.equal(await page.getByLabel('Скорость ответа').inputValue(), 'fast');
    const actions = await page.evaluate(() => window.calls.filter((c) => c.command === 'save_settings').at(-1).params.settings.actions);
    assert.equal(actions[1].id, 'brief');
    assert.equal(actions[1].prompt, 'Новый промпт тезисов');
    assert.equal(actions[2].shortcut, '');
    assert(actions[2].id && actions[2].id !== actions[0].id);
    await page.getByRole('button', { name: 'Перегенерить ответ', exact: true }).click();
    assert.equal(await page.locator('#answer-content').textContent(), 'Краткий ответ');
    assert.equal(await page.locator('#answer-model').getAttribute('title'), 'Скорость запроса: Standard');
    assert.equal(await page.locator('#regenerate-button').isEnabled(), false);
    await page.getByLabel('Скорость ответа').selectOption('standard');
    assert.equal(await page.locator('#answer-boundary').textContent(), 'Тезисы · до #002');
    assert.equal(await page.locator('#record-label').textContent(), 'Идёт запись');
    assert.deepEqual(await page.evaluate(() => window.calls.filter((c) => c.command === 'regenerate_answer').map((c) => c.params)), [{ sessionId: '1793000000000-1', answerId: 2 }]);
    assert.equal(await page.evaluate(() => window.calls.filter((c) => c.command === 'submit_segment').length), 3);
    await page.evaluate(() => window.finishAnswer(2, null, 'test failure'));
    assert.equal(await page.locator('#answer-content').textContent(), 'Краткий ответ');
    assert((await page.locator('#answer-status').textContent()).includes('Предыдущий сохранён'));
    await page.getByRole('button', { name: '■ Остановить', exact: true }).click();
    assert.equal(await page.locator('#process-button').isEnabled(), false);
    assert.equal(await page.locator('#prompt-actions button').first().isEnabled(), false);
    assert.equal(await page.locator('#regenerate-button').isEnabled(), true);
    await page.getByLabel('Reasoning', { exact: true }).selectOption('medium');
    await page.getByRole('button', { name: 'Перегенерить ответ', exact: true }).click();
    await page.evaluate(() => window.finishAnswer(2, 'Новый ответ с medium'));
    assert.equal(await page.locator('#answer-content').textContent(), 'Новый ответ с medium');
    assert.equal(await page.locator('#answer-model').textContent(), '✦ Черновик GPT-6-Astra · medium');
    assert.equal(await page.locator('#answer-model').getAttribute('title'), 'Скорость запроса: Standard');
    fs.mkdirSync(path.join(__dirname, '..', 'test-results'), { recursive: true });
    await page.screenshot({ path: path.join(__dirname, '..', 'test-results', 'actions-ui.png') });
    await page.getByRole('button', { name: 'Настройки', exact: false }).click();
    assert.equal(await page.locator('.action-editor').count(), 3);
    assert.equal(await page.locator('.action-editor').nth(1).getByLabel('Промпт', { exact: true }).inputValue(), 'Новый промпт тезисов');
    await page.screenshot({ path: path.join(__dirname, '..', 'test-results', 'actions-settings.png') });
    await page.getByRole('button', { name: 'Закрыть настройки', exact: true }).click();
    await page.evaluate(() => window.manyActions());
    for (const width of [980, 1200, 1440]) {
      await page.setViewportSize({ width, height: 650 });
      assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `Overflow at ${width}`);
      const panel = await page.locator('.answer-panel').boundingBox();
      const regenerate = await page.locator('#regenerate-button').boundingBox();
      assert(regenerate.y + regenerate.height <= panel.y + panel.height, 'Regenerate button must fit inside the answer panel');
      if (width === 980) await page.screenshot({ path: path.join(__dirname, '..', 'test-results', 'actions-small.png') });
      const answerHeight = (await page.locator('#answer-content').boundingBox()).height;
      assert(answerHeight >= 60, `Answer text must retain reading space: ${answerHeight}px at width ${width}`);
      assert(await page.locator('#prompt-actions').evaluate((el) => el.scrollHeight > el.clientHeight), 'Many actions must scroll');
      assert(await page.locator('#prompt-actions button').evaluateAll((buttons) => buttons.every((button) => button.scrollHeight <= button.clientHeight)), 'Long action names must fit inside their buttons');
      const [promptBox, footerBox] = await Promise.all([page.locator('.prompt-preview').boundingBox(), page.locator('.sidebar-bottom').boundingBox()]);
      assert(promptBox.y + promptBox.height <= footerBox.y + 1, 'Sidebar sections must not overlap');
    }
    assert.deepEqual(errors, []);
  } finally {
    if (browser) await browser.close();
    await new Promise((resolve) => server.close(resolve));
  }
});
