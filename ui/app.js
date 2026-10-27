const $ = (id) => document.getElementById(id);
const api = window.__TAURI__;
let state = { settings: {}, session: null, sessions: [], recording: false, samples: 0 };
let devices = null;
let models = [];
const defaultModel = 'gpt-6-luna';
const defaultEffort = 'low';
const defaultSpeed = 'standard';
let recordCommand = false;
let modelSettingsSaving = false;
let regenerationStarting = false;
let copyText = '';
let selectedAnswerId = null;
let answerSessionId = null;
let latestAnswerId = null;

function node(tag, text = '', className = '') {
  const element = document.createElement(tag);
  element.textContent = text;
  if (className) element.className = className;
  return element;
}
function clock(samples) {
  const seconds = Math.max(0, Math.floor((samples || 0) / 24000));
  const minutes = Math.floor(seconds / 60);
  return `${String(minutes).padStart(2, '0')}:${String(seconds % 60).padStart(2, '0')}`;
}
function seq(number) { return `#${String(number).padStart(3, '0')}`; }
function modelLabel(id) {
  return id === defaultModel ? 'Luna' : (models.find((model) => model.id === id)?.label || id);
}
function fillReasoning(modelId, selected) {
  const model = models.find((item) => item.id === modelId);
  const efforts = model?.reasoning_efforts?.length ? model.reasoning_efforts : [selected || defaultEffort];
  const select = $('reasoning-select');
  select.replaceChildren(...efforts.map((effort) => new Option(effort, effort)));
  select.value = efforts.includes(selected) ? selected : (efforts.includes(defaultEffort) ? defaultEffort : (model?.default_reasoning_effort || efforts[0]));
}
function fillModels(selected = state.settings.model || defaultModel, effort = state.settings.reasoning_effort || defaultEffort) {
  const available = [...models];
  if (!available.some((model) => model.id === selected)) available.unshift({ id: selected, label: modelLabel(selected) });
  $('model-select').replaceChildren(...available.map((model) => new Option(model.label, model.id)));
  $('model-select').value = selected;
  fillReasoning(selected, effort);
}
function empty(title, description) {
  const element = node('div', '', 'empty');
  element.append(node('strong', title), node('p', description));
  return element;
}
function message(error) {
  $('alert-text').textContent = String(error);
  $('alert').hidden = false;
}
async function invoke(command, params = {}) {
  if (!api) throw new Error('Откройте настольное приложение Interview Helper. Эта страница сама не записывает звук.');
  return api.core.invoke(command, params);
}
async function act(command, params = {}) {
  try { return await invoke(command, params); }
  catch (error) { message(error); return null; }
}
function updateLevel(samples = state.samples, peak = 0) {
  state.samples = samples;
  const segments = state.session?.segments || [];
  const current = segments.find((segment) => segment.end_sample === null);
  const final = segments.at(-1)?.end_sample || 0;
  $('total-clock').textContent = clock(Math.max(samples || 0, final));
  $('segment-clock').textContent = current ? clock(samples - current.start_sample) : '00:00';
  $('audio-level').value = state.recording ? Math.min(1, peak) : 0;
  $('current-segment').textContent = current && state.recording ? `Записывается отрезок ${seq(current.seq)}` : 'Запись выключена';
  $('segment-range').textContent = current && state.recording ? `${clock(current.start_sample)} — сейчас` : 'Начните новую встречу, когда будете готовы';
}
function segmentStatus(segment) {
  if (segment.end_sample === null) return ['recording', 'Записывается'];
  if (segment.transcription_status === 'failed') return ['failed', 'Ошибка распознавания'];
  if (segment.transcription_status === 'running') return ['running', 'Распознаётся'];
  if (segment.transcription_status === 'waiting') return ['waiting', 'В очереди'];
  return ['done', 'В контексте'];
}
function renderSegments(segments) {
  const list = $('segments-list');
  list.replaceChildren();
  if (!segments.length) { list.append(node('p', 'Первый отрезок появится после начала записи.', 'muted')); return; }
  for (const segment of [...segments].reverse()) {
    const [status, label] = segmentStatus(segment);
    const row = node('article', '', `segment-row ${status}`);
    const line = node('div', '', 'segment-line');
    line.append(node('strong', seq(segment.seq)), node('span', `${clock(segment.start_sample)} — ${segment.end_sample === null ? 'сейчас' : clock(segment.end_sample)}`, 'time'), node('span', label, `status ${status}`));
    row.append(line);
    const errors = [segment.transcription_error].filter(Boolean);
    if (errors.length) {
      const detail = node('div', errors.join('\n'), 'segment-error');
      const retry = node('button', 'Повторить');
      retry.type = 'button';
      retry.addEventListener('click', () => act('retry_segment', { seq: segment.seq }));
      detail.append(retry); row.append(detail);
    }
    list.append(row);
  }
}
function renderTranscript(segments) {
  const closed = segments.filter((segment) => segment.end_sample !== null);
  $('context-boundary').textContent = closed.length ? `Распознано: ${closed.filter((segment) => segment.transcription_status === 'done').length} из ${closed.length}` : '';
  const list = $('transcripts');
  list.replaceChildren();
  if (!segments.length) { list.append(empty('Здесь будет весь разговор', 'Расшифровки появятся после отправки отрезков и сохранят порядок записи.')); return; }
  for (const segment of segments) {
    const article = node('article', '', 'transcript');
    article.append(node('h3', `${seq(segment.seq)} · ${clock(segment.start_sample)} — ${segment.end_sample === null ? 'сейчас' : clock(segment.end_sample)}`));
    article.append(node('p', segment.text ?? segmentStatus(segment)[1], segment.text === null ? 'muted' : ''));
    if (segment.text === '') article.lastChild.textContent = 'Речь не распознана.';
    list.append(article);
  }
}
function selectedAnswer() {
  const answers = state.session?.answers || [];
  return answers.find((answer) => answer.id === selectedAnswerId) || answers.at(-1);
}
function renderAnswer(answers) {
  const latest = answers.at(-1)?.id ?? null;
  const sessionId = state.session?.id ?? null;
  if (sessionId !== answerSessionId || latest !== latestAnswerId) {
    selectedAnswerId = latest;
    latestAnswerId = latest;
    answerSessionId = sessionId;
  }
  const requested = selectedAnswer();
  const options = requested?.options;
  const saved = requested?.text ? { text: requested.text, model: options.model, reasoning_effort: options.reasoning_effort, speed: options.speed } : requested?.history?.at(-1);
  const model = saved?.model || options?.model || state.settings.model || defaultModel;
  const effort = saved?.reasoning_effort || options?.reasoning_effort || state.settings.reasoning_effort || defaultEffort;
  $('answer-model').textContent = `✦ Черновик ${modelLabel(model)} · ${effort}`;
  $('answer-model').title = `Скорость запроса: ${(saved?.speed || options?.speed || state.settings.speed) === 'fast' ? 'Fast' : 'Standard'}`;
  $('answer-selector').hidden = answers.length < 2;
  const picker = $('answer-picker');
  const signature = JSON.stringify(answers.map((a) => [a.id, a.options.action.name, a.through_seq]));
  if (picker.dataset.signature !== signature) {
    picker.replaceChildren(...[...answers].reverse().map((a) => new Option(`${a.options.action.name} · до ${seq(a.through_seq)}`, a.id)));
    picker.dataset.signature = signature;
  }
  picker.value = requested?.id ?? '';
  const content = $('answer-content');
  $('answer-boundary').textContent = requested ? `${options.action.name} · до ${seq(requested.through_seq)}` : '';
  copyText = saved?.text || '';
  $('copy-answer').disabled = !copyText;
  $('regenerate-button').disabled = modelSettingsSaving || regenerationStarting || !requested || !['done', 'failed'].includes(requested.status);
  $('regenerate-button').title = requested ? `${options.action.name}: повторный ответ по контексту до ${seq(requested.through_seq)}` : 'Сначала запросите ответ';
  content.classList.toggle('placeholder', !copyText);
  if (copyText) {
    content.textContent = copyText;
    if (requested.status === 'done') $('answer-status').textContent = 'Черновик модели. Прочитайте и при необходимости уточните.';
    else if (requested.status === 'failed') $('answer-status').textContent = 'Новый ответ получить не удалось. Предыдущий сохранён; можно повторить с другими настройками.';
    else $('answer-status').textContent = `Готовится новый ответ: ${modelLabel(options.model)} / ${options.reasoning_effort}. Пока показан предыдущий.`;
  } else if (!requested) {
    content.textContent = 'Нужна помощь с ответом?\n\nВыберите действие ниже. Приложение распознает текущий отрезок и выполнит его промпт по истории разговора.';
    $('answer-status').textContent = 'Подсказка хранится отдельно от сказанного.';
  } else if (requested.status === 'failed') {
    content.textContent = requested.error || 'Не удалось подготовить ответ. Можно повторить с другими настройками.';
    $('answer-status').textContent = 'Запрос можно перегенерировать, не включая запись.';
  } else {
    content.textContent = requested.status === 'running' ? `${modelLabel(options.model)} готовит ответ…` : `Ждём расшифровки до ${seq(requested.through_seq)}…`;
    $('answer-status').textContent = `В ответ войдёт контекст до ${seq(requested.through_seq)}.${state.recording ? ' Запись продолжается.' : ''}`;
  }
}
function renderPromptActions() {
  const actions = state.settings.actions || [];
  const list = $('prompt-actions');
  const signature = JSON.stringify(actions);
  if (list.dataset.signature !== signature) {
    list.replaceChildren();
    for (const action of actions) {
      const button = node('button', '', 'answer-button');
      button.type = 'button'; button.dataset.actionId = action.id;
      button.append(node('span', action.name));
      if (action.shortcut) button.append(node('kbd', action.shortcut.replaceAll('+', ' + ')));
      button.addEventListener('click', () => act('submit_segment', { actionId: action.id }));
      list.append(button);
    }
    if (!actions.length) {
      const add = node('button', 'Добавить кнопку с промптом', 'text-button');
      add.type = 'button'; add.addEventListener('click', () => openSettings(true)); list.append(add);
    }
    list.dataset.signature = signature;
  }
  for (const button of list.querySelectorAll('[data-action-id]')) button.disabled = !state.recording || modelSettingsSaving;
}
function renderHistory() {
  const picker = $('session-picker');
  const existing = [...picker.options].slice(1).map((option) => option.value);
  const desired = [...state.sessions].reverse();
  if (JSON.stringify(existing) !== JSON.stringify(desired)) {
    picker.replaceChildren(new Option('Сессии этого запуска', ''));
    for (const id of desired) {
      const date = new Date(Number(id.split('-')[0]));
      picker.add(new Option(date.toLocaleString('ru-RU', { dateStyle: 'short', timeStyle: 'short' }), id));
    }
  }
  picker.value = state.session?.id || '';
  picker.disabled = state.recording;
}
function render(payload) {
  state = { ...state, ...payload };
  const segments = state.session?.segments || [];
  $('session-title').textContent = state.session ? `Встреча · ${new Date(state.session.created_ms).toLocaleDateString('ru-RU', { day: 'numeric', month: 'long' })}` : 'Новая встреча';
  $('record-toggle').textContent = state.recording ? '■ Остановить' : '● Начать';
  $('record-toggle').classList.toggle('stop', state.recording);
  $('record-toggle').disabled = recordCommand;
  $('record-status').classList.toggle('live', state.recording);
  $('record-label').textContent = state.recording ? 'Идёт запись' : 'Запись выключена';
  $('process-button').disabled = !state.recording;
  $('action-caption').textContent = state.recording ? 'Запись продолжается после нажатия.' : (state.session?.answers?.length ? 'Перегенерация доступна и при выключенной записи.' : 'Нажмите «Начать», чтобы записывать новую встречу.');
  $('cut-key').textContent = (state.settings.cut_shortcut || '').replaceAll('+', ' + ');
  if (!modelSettingsSaving && ($('model-select').value !== (state.settings.model || defaultModel) || $('reasoning-select').value !== (state.settings.reasoning_effort || defaultEffort))) fillModels();
  $('model-select').disabled = modelSettingsSaving;
  $('reasoning-select').disabled = modelSettingsSaving;
  if (!modelSettingsSaving) $('speed-select').value = state.settings.speed || defaultSpeed;
  $('speed-select').disabled = modelSettingsSaving;
  $('prompt-preview').textContent = (state.settings.actions || []).map((action) => action.name).join(' · ') || 'Добавьте действия в настройках.';
  $('response-model').textContent = `Ответ · ${modelLabel(state.settings.model || defaultModel)} · ${state.settings.reasoning_effort || defaultEffort}`;
  $('open-context').disabled = !state.session;
  if (state.error) message(state.error);
  else if (state.hotkey_error) message(`Глобальные клавиши недоступны: ${state.hotkey_error}`);
  renderHistory(); renderSegments(segments); renderTranscript(segments); renderPromptActions(); renderAnswer(state.session?.answers || []); updateLevel(state.samples, state.peak || 0);
  renderSources();
}
function renderSources() {
  if (!devices) return;
  const micName = state.recording ? state.session?.microphone : (state.settings.microphone || devices.default_microphone);
  const outputName = state.recording ? state.session?.output : (state.settings.output || devices.default_output);
  const mic = devices.microphones.find((d) => d.name === micName)?.description || 'Микрофон не выбран';
  const output = devices.outputs.find((d) => d.name === outputName)?.description || 'Выход не выбран';
  $('source-description').textContent = `${mic} · ${output}`;
}
async function refreshDevices() { devices = await invoke('audio_devices'); renderSources(); }
function addActionEditor(action) {
  const row = node('fieldset', '', 'action-editor'); row.dataset.actionId = action.id;
  const legend = node('legend', action.name || 'Новая кнопка'); row.append(legend);
  const fields = node('div', '', 'action-editor-fields');
  for (const [key, title, value] of [['name', 'Название кнопки', action.name], ['shortcut', 'Горячая клавиша', action.shortcut], ['prompt', 'Промпт', action.prompt]]) {
    const label = node('label', title);
    const input = node(key === 'prompt' ? 'textarea' : 'input');
    input.dataset.field = key; input.value = value || ''; input.required = key !== 'shortcut';
    if (key === 'prompt') { input.rows = 3; input.maxLength = 16000; label.className = 'action-prompt-field'; }
    if (key === 'name') { input.maxLength = 64; input.addEventListener('input', () => { legend.textContent = input.value || 'Новая кнопка'; }); }
    if (key === 'shortcut') { input.placeholder = 'Например, Ctrl+Alt+K'; input.autocomplete = 'off'; input.spellcheck = false; }
    label.append(input); fields.append(label);
  }
  row.append(fields);
  const remove = node('button', 'Удалить кнопку', 'text-button remove-action'); remove.type = 'button';
  remove.addEventListener('click', () => { row.remove(); $('add-action').focus(); }); row.append(remove);
  $('action-editors').append(row);
  return row;
}
function editedActions() {
  return [...$('action-editors').children].map((row) => ({
    id: row.dataset.actionId,
    name: row.querySelector('[data-field="name"]').value.trim(),
    shortcut: row.querySelector('[data-field="shortcut"]').value.trim(),
    prompt: row.querySelector('[data-field="prompt"]').value,
  }));
}
async function openSettings(prompt = false) {
  $('settings-message').textContent = '';
  $('settings-message').classList.remove('error');
  try { await refreshDevices(); }
  catch (error) { $('settings-message').textContent = String(error); }
  for (const [id, list, selected] of [
    ['microphone-select', devices?.microphones || [], state.settings.microphone || devices?.default_microphone],
    ['output-select', devices?.outputs || [], state.settings.output || devices?.default_output],
  ]) {
    $(id).replaceChildren(...list.map((d) => new Option(d.description, d.name)));
    $(id).value = selected || '';
  }
  $('cut-shortcut').value = state.settings.cut_shortcut || '';
  $('action-editors').replaceChildren();
  for (const action of state.settings.actions || []) addActionEditor(action);
  $('hotkey-status').textContent = state.hotkey_error || 'Глобальные клавиши зарегистрированы в Hyprland.';
  $('settings-dialog').showModal();
  if (prompt) ($('action-editors').querySelector('textarea') || $('add-action')).focus();
}
async function checkConnection() {
  $('connection-state').textContent = 'Проверка подключения…';
  $('check-connection').disabled = true;
  try {
    const result = await invoke('check_codex');
    models = result.models || [];
    fillModels($('model-select').value || state.settings.model || defaultModel, $('reasoning-select').value || state.settings.reasoning_effort || defaultEffort);
    const selected = state.settings.model || defaultModel;
    const available = models.some((model) => model.id === selected);
    const text = result.signed_in && result.transcription_login ? 'Вход Codex/ChatGPT подключён' : 'Проверьте вход в Codex';
    $('connection-state').textContent = text;
    $('connection-detail').textContent = result.transcription_login ? `Используется существующий вход Codex/ChatGPT. ${modelLabel(selected)} ${available ? 'есть в каталоге моделей' : 'не найдена в каталоге моделей'}.` : 'Выполните вход в Codex через ChatGPT, затем нажмите «Проверить».';
  } catch (error) { $('connection-state').textContent = 'Codex недоступен'; $('connection-detail').textContent = String(error); }
  finally { $('check-connection').disabled = false; }
}

async function saveModelSelection() {
  if (modelSettingsSaving) return;
  const model = $('model-select').value;
  const reasoningEffort = $('reasoning-select').value;
  const speed = $('speed-select').value;
  modelSettingsSaving = true;
  render(state);
  try { await invoke('save_model_settings', { model, reasoningEffort, speed }); render(await invoke('snapshot')); }
  catch (error) { message(error); }
  finally { modelSettingsSaving = false; fillModels(); render(state); }
}
$('regenerate-button').addEventListener('click', async () => {
  const requested = selectedAnswer();
  if (!requested || modelSettingsSaving || regenerationStarting) return;
  const sessionId = state.session.id;
  regenerationStarting = true;
  render(state);
  try { await act('regenerate_answer', { sessionId, answerId: requested.id }); }
  finally { regenerationStarting = false; render(await invoke('snapshot')); }
});

$('record-toggle').addEventListener('click', async () => {
  if (recordCommand) return;
  recordCommand = true; $('record-toggle').disabled = true;
  try { await act(state.recording ? 'stop_recording' : 'start_recording'); }
  finally { recordCommand = false; render(await invoke('snapshot')); }
});
$('process-button').addEventListener('click', () => act('submit_segment', { actionId: null }));
$('answer-picker').addEventListener('change', () => { selectedAnswerId = Number($('answer-picker').value); renderAnswer(state.session?.answers || []); });
$('add-action').addEventListener('click', () => { const row = addActionEditor({ id: crypto.randomUUID(), name: '', prompt: '', shortcut: '' }); row.querySelector('input').focus(); });
$('session-picker').addEventListener('change', () => { if ($('session-picker').value) act('select_session', { id: $('session-picker').value }); });
$('open-context').addEventListener('click', () => act('open_context_file'));
$('open-folder').addEventListener('click', () => act('open_data_folder'));
$('settings-button').addEventListener('click', () => openSettings());
$('edit-prompt').addEventListener('click', () => openSettings(true));
$('close-settings').addEventListener('click', () => $('settings-dialog').close());
$('check-connection').addEventListener('click', checkConnection);
$('model-select').addEventListener('change', () => { fillReasoning($('model-select').value, $('reasoning-select').value); saveModelSelection(); });
$('reasoning-select').addEventListener('change', saveModelSelection);
$('speed-select').addEventListener('change', saveModelSelection);
$('dismiss-alert').addEventListener('click', () => { $('alert').hidden = true; act('clear_error'); });
$('settings-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = event.submitter; button.disabled = true;
  try {
    await invoke('save_settings', { settings: { microphone: $('microphone-select').value, output: $('output-select').value, actions: editedActions(), cut_shortcut: $('cut-shortcut').value } });
    render(await invoke('snapshot'));
    if (!state.error && !state.hotkey_error) $('alert').hidden = true;
    $('settings-dialog').close();
  } catch (error) { $('settings-message').textContent = String(error); $('settings-message').classList.add('error'); $('settings-message').scrollIntoView({ block: 'nearest' }); }
  finally { button.disabled = false; }
});
$('copy-answer').addEventListener('click', async () => {
  if (!copyText) return;
  try { await navigator.clipboard.writeText(copyText); $('answer-status').textContent = 'Ответ скопирован.'; }
  catch { const field = node('textarea', copyText); document.body.append(field); field.select(); const copied = document.execCommand('copy'); field.remove(); $('answer-status').textContent = copied ? 'Ответ скопирован.' : 'Выделите текст ответа и нажмите Ctrl+C.'; }
});

if (api) {
  await api.event.listen('state', ({ payload }) => render(payload));
  await api.event.listen('level', ({ payload }) => updateLevel(payload.samples, payload.peak));
  render(await invoke('snapshot'));
  refreshDevices().catch(message);
  checkConnection();
} else {
  $('record-toggle').disabled = true;
  message('Это интерфейс настольного приложения. Запустите Interview Helper через ярлык приложения.');
}
