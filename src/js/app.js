// Главное окно: главная, история, словарь, настройки.
//
// Всё состояние приходит одной командой `get_state`; после каждой диктовки
// Rust шлёт событие `history`, и окно перечитывает его целиком — данных мало,
// а одна дорога обновления проще трёх.

const tauri = window.__TAURI__ ?? demo();
const { invoke } = tauri.core;
const { listen } = tauri.event;

const $ = (id) => document.getElementById(id);

const LANGUAGES = [
  ['auto', 'Авто'], ['ru', 'Русский'], ['en', 'English'], ['uk', 'Українська'], ['be', 'Беларуская'],
  ['kk', 'Қазақша'], ['de', 'Deutsch'], ['fr', 'Français'], ['es', 'Español'], ['it', 'Italiano'],
  ['pt', 'Português'], ['pl', 'Polski'], ['tr', 'Türkçe'], ['zh', '中文'], ['ja', '日本語'], ['ko', '한국어'],
];

/** Скорость набора на клавиатуре, слов в минуту, — для «сэкономлено». */
const TYPING_SPEED = 40;

let state = null;
/** Новый ключ сервиса правки, если его только что ввели; иначе null — «не менять». */
let newKey = null;
let saveTimer = 0;
let dictionaryFocused = false;

/* ── Общее ── */

function fill(select, options, value) {
  select.replaceChildren(...options.map(([id, title]) => new Option(title, id)));
  select.value = value;
  if (select.selectedIndex < 0) select.selectedIndex = 0;
}

function plural(n, one, few, many) {
  const tens = n % 100;
  const ones = n % 10;
  if (tens >= 11 && tens <= 14) return many;
  if (ones === 1) return one;
  if (ones >= 2 && ones <= 4) return few;
  return many;
}

function when(iso) {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return '';
  const today = new Date();
  const time = date.toLocaleTimeString('ru', { hour: '2-digit', minute: '2-digit' });
  if (date.toDateString() === today.toDateString()) return time;
  return `${date.toLocaleDateString('ru', { day: 'numeric', month: 'short' })}, ${time}`;
}

/* ── Навигация ── */

$('nav').addEventListener('click', (event) => {
  const page = event.target.dataset.page;
  if (!page) return;
  for (const button of $('nav').children) button.classList.toggle('on', button === event.target);
  for (const section of document.querySelectorAll('.page')) {
    section.classList.toggle('on', section.id === `page-${page}`);
  }
});

/* ── Главная ── */

function renderHome() {
  $('hero-key').textContent = state.hotkeyTitle;
  $('paused-card').hidden = !state.paused;
  $('version').textContent = `версия ${state.version}`;

  const chip = $('engine-chip');
  chip.className = `chip${state.loaded ? ' live' : ''}${state.installed ? '' : ' need'}`;
  chip.textContent = !state.installed
    ? 'модель не скачана'
    : `${state.gpu ? 'видеокарта' : 'процессор'} · ${state.loaded ? 'модель в памяти' : 'модель спит'}`;

  const need = !state.installed;
  $('install-card').hidden = !need;
  if (need) {
    $('install-text').textContent =
      `${state.model.title}, около ${state.model.megabytes} МБ. Дальше программа работает без интернета: ` +
      'сказанное не покидает компьютер.';
    $('install-button').hidden = state.installing;
    $('install-progress').hidden = !state.installing;
  }

  const { words, seconds } = state.totals;
  $('stat-words').textContent = words.toLocaleString('ru');
  $('stat-speed').textContent = seconds > 5 ? Math.round(words / (seconds / 60)) : '—';
  const saved = Math.max(0, Math.round(words / TYPING_SPEED - seconds / 60));
  $('stat-saved').textContent = saved >= 90 ? `${(saved / 60).toFixed(1).replace('.', ',')} ч` : `${saved} мин`;

  const recent = state.history.slice(0, 3);
  $('recent-block').hidden = recent.length === 0;
  $('recent').replaceChildren(...recent.map(itemNode));
}

function itemNode(item) {
  const li = document.createElement('li');

  const meta = document.createElement('div');
  meta.className = 'meta';
  const parts = [when(item.at), item.app, `${item.words} ${plural(item.words, 'слово', 'слова', 'слов')}`];
  meta.replaceChildren(...parts.filter(Boolean).map((part) => {
    const span = document.createElement('span');
    span.textContent = part;
    return span;
  }));

  const text = document.createElement('div');
  text.className = 'text';
  text.textContent = item.text;
  if (item.raw) text.title = `Расслышано: ${item.raw}`;

  const tools = document.createElement('div');
  tools.className = 'tools';
  const copy = document.createElement('button');
  copy.textContent = 'Копировать';
  copy.addEventListener('click', async () => {
    await invoke('copy_text', { text: item.text });
    copy.textContent = 'Скопировано';
    setTimeout(() => { copy.textContent = 'Копировать'; }, 1200);
  });
  const remove = document.createElement('button');
  remove.textContent = 'Удалить';
  remove.addEventListener('click', () => invoke('history_delete', { id: item.id }));
  tools.append(copy, remove);

  li.append(meta, text, tools);
  return li;
}

$('install-button').addEventListener('click', install);

async function install() {
  $('install-error').hidden = true;
  $('install-button').hidden = true;
  $('install-progress').hidden = false;
  try {
    await invoke('install_engine');
  } catch (error) {
    $('install-error').textContent = `Не получилось: ${error}. Загрузка продолжится с того же места.`;
    $('install-error').hidden = false;
    $('install-button').textContent = 'Продолжить';
  }
  await refresh();
}

listen('install', ({ payload }) => {
  $('install-fill').style.width = `${payload.percent}%`;
  $('install-status').textContent = payload.status;
});

listen('need-install', () => {
  $('nav').querySelector('[data-page="home"]').click();
});

/* ── История ── */

function renderHistory() {
  const query = $('history-search').value.trim().toLowerCase();
  const items = query
    ? state.history.filter((item) => item.text.toLowerCase().includes(query))
    : state.history;
  $('history').replaceChildren(...items.map(itemNode));
  $('history-empty').hidden = items.length > 0;
  $('history-empty').textContent = query
    ? 'Ничего не нашлось.'
    : 'Здесь появится то, что вы надиктуете.';
  $('history-clear').disabled = state.history.length === 0;
}

$('history-search').addEventListener('input', renderHistory);

$('history-clear').addEventListener('click', async () => {
  const button = $('history-clear');
  // Второе нажатие подтверждает: история стирается насовсем.
  if (button.dataset.sure !== '1') {
    button.dataset.sure = '1';
    button.textContent = 'Точно очистить?';
    setTimeout(() => { button.dataset.sure = ''; button.textContent = 'Очистить'; }, 3000);
    return;
  }
  button.dataset.sure = '';
  button.textContent = 'Очистить';
  await invoke('history_clear');
});

/* ── Словарь ── */

function renderDictionary() {
  // Пока человек печатает в словаре, список не перестраиваем: поле под
  // курсором пересоздалось бы и потеряло фокус.
  if (dictionaryFocused) return;
  const list = state.config.dictionary;
  $('dictionary-empty').hidden = list.length > 0;
  $('dictionary').replaceChildren(...list.map((entry, index) => {
    const pair = document.createElement('div');
    pair.className = 'pair';

    const from = document.createElement('input');
    from.type = 'text';
    from.placeholder = 'как звучит или пишется';
    from.value = entry.from;
    from.addEventListener('input', () => { list[index].from = from.value; queueSave(); });

    const arrow = document.createElement('span');
    arrow.textContent = '→';

    const to = document.createElement('input');
    to.type = 'text';
    to.placeholder = 'заменить на (необязательно)';
    to.value = entry.to;
    to.addEventListener('input', () => { list[index].to = to.value; queueSave(); });

    const remove = document.createElement('button');
    remove.textContent = 'Убрать';
    remove.addEventListener('click', () => {
      list.splice(index, 1);
      dictionaryFocused = false;
      renderDictionary();
      queueSave();
    });

    pair.append(from, arrow, to, remove);
    return pair;
  }));
}

$('dictionary').addEventListener('focusin', () => { dictionaryFocused = true; });
$('dictionary').addEventListener('focusout', () => { dictionaryFocused = false; });

$('dictionary-add').addEventListener('click', () => {
  state.config.dictionary.push({ from: '', to: '' });
  dictionaryFocused = false;
  renderDictionary();
  $('dictionary').lastElementChild?.querySelector('input')?.focus();
});

/* ── Настройки ── */

const SIMPLE = [
  ['hotkey', 'value'], ['language', 'value'], ['microphone', 'value'], ['model', 'value'],
  ['insert', 'value'], ['sounds', 'checked'], ['stream', 'checked'], ['livePreview', 'checked'], ['trailingSpace', 'checked'],
  ['voiceCommands', 'checked'], ['autostart', 'checked'], ['keepHistory', 'checked'],
];

function renderSettings() {
  const config = state.config;
  // Поле, в котором сейчас курсор, не трогаем — иначе текст прыгал бы под руками.
  const busy = document.activeElement;
  // А раскрытый список не перестраиваем вовсе: он закрылся бы на глазах.
  if (busy?.tagName === 'SELECT' && $('page-settings').contains(busy)) return;

  // Своё сочетание — отдельной строкой списка: к готовому можно вернуться,
  // не теряя из виду, что было назначено.
  const hotkeys = state.hotkeys.map((key) => [key.id, key.title]);
  if (config.hotkey.startsWith('keys:')) hotkeys.push([config.hotkey, `Своя: ${state.hotkeyTitle}`]);
  fill($('set-hotkey'), hotkeys, config.hotkey);
  fill($('set-language'), LANGUAGES, config.language);
  fill($('set-microphone'), [['', 'Основной в системе'], ...state.devices.map((name) => [name, name])], config.microphone);
  fill($('set-model'), [
    ['auto', `Авто — ${state.gpu ? 'точная, на видеокарте' : 'быстрая, на процессоре'}`],
    ...state.models.map((model) => [
      model.id,
      `${model.title} · ${model.megabytes} МБ${model.installed ? ' · скачана' : ''}`,
    ]),
  ], config.model);
  $('model-note').textContent = state.installed
    ? ''
    : 'Эта модель ещё не скачана — кнопка загрузки на главной';

  for (const [key, property] of SIMPLE) {
    const node = $(`set-${key}`);
    if (node !== busy) node[property] = config[key];
  }
  $('set-unloadMinutes').value = String(config.unloadMinutes);
  $('loaded-text').textContent = state.loaded ? 'в памяти' : 'не загружена';
  $('unload').disabled = !state.loaded;

  const polish = config.polish;
  $('set-polish-mode').value = polish.mode;
  $('polish-fields').hidden = polish.mode === 'off';
  $('polish-key-row').hidden = polish.mode !== 'openai';
  if ($('set-polish-url') !== busy) $('set-polish-url').value = polish.url;
  if ($('set-polish-model') !== busy) $('set-polish-model').value = polish.model;
  if ($('set-polish-instructions') !== busy) $('set-polish-instructions').value = polish.instructions;
  if ($('set-polish-key') !== busy) {
    $('set-polish-key').placeholder = state.hasKey ? 'ключ сохранён' : 'ключ сервиса';
  }
}

function collect() {
  const config = state.config;
  for (const [key, property] of SIMPLE) config[key] = $(`set-${key}`)[property];
  config.unloadMinutes = Number($('set-unloadMinutes').value);

  const polish = config.polish;
  const mode = $('set-polish-mode').value;
  if (mode !== polish.mode) {
    // У каждого способа свой адрес по умолчанию; модель от другого не годится.
    polish.url = mode === 'openai' ? 'https://api.openai.com/v1' : 'http://127.0.0.1:11434';
    polish.model = '';
    $('set-polish-url').value = polish.url;
    $('set-polish-model').value = '';
  } else {
    polish.url = $('set-polish-url').value.trim();
    polish.model = $('set-polish-model').value.trim();
  }
  polish.mode = mode;
  polish.instructions = $('set-polish-instructions').value;
}

function queueSave() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(save, 350);
}

async function save() {
  // Пустые строки словаря — заготовки, которые ещё заполняют; на диск не идут.
  const config = structuredClone(state.config);
  config.dictionary = config.dictionary.filter((entry) => entry.from.trim() || entry.to.trim());
  config.onboarded = true;
  try {
    await invoke('save_config', { config, key: newKey });
    if (newKey !== null) {
      newKey = null;
      $('set-polish-key').value = '';
    }
    $('saved').hidden = false;
    setTimeout(() => { $('saved').hidden = true; }, 1200);
  } catch (error) {
    console.error('настройки не сохранились', error);
  }
  await refresh();
}

$('page-settings').addEventListener('change', async (event) => {
  if (event.target.id === 'set-polish-key') {
    newKey = event.target.value;
  }
  collect();
  $('polish-fields').hidden = state.config.polish.mode === 'off';
  $('polish-key-row').hidden = state.config.polish.mode !== 'openai';
  if (event.target.id === 'set-polish-mode' || event.target.id === 'set-polish-url') await loadPolishModels();
  queueSave();
});

/* Своя клавиша: программа ждёт нажатия и сама сообщает, что нажали. */

const HOTKEY_NOTE = 'Удержание — запись, двойное нажатие — без рук';
let capturing = false;

function showCapture(on, note = HOTKEY_NOTE, bad = false) {
  capturing = on;
  const button = $('hotkey-capture');
  button.classList.toggle('on', on);
  button.textContent = on ? 'Отмена' : 'Своя…';
  $('hotkey-note').textContent = note;
  $('hotkey-note').classList.toggle('bad', bad);
}

$('hotkey-capture').addEventListener('click', async (event) => {
  // Иначе щелчок дошёл бы до обработчика настроек как изменение.
  event.stopPropagation();
  if (capturing) {
    await invoke('cancel_capture');
    return;
  }
  showCapture(true, 'Нажмите клавишу или сочетание и отпустите. Esc — отмена');
  await invoke('capture_hotkey');
});

listen('hotkey:captured', async ({ payload }) => {
  if (payload.error) {
    showCapture(false, payload.error, true);
    return;
  }
  showCapture(false);
  if (payload.cancelled) return;
  state.config.hotkey = payload.id;
  state.hotkeyTitle = payload.title;
  // Список перестраивается сразу: в нём ещё нет строки с новым сочетанием.
  $('set-hotkey').blur();
  renderSettings();
  await save();
});

/* Проверка микрофона: запись, расшифровка и громкость — без вставки куда-либо. */

$('mic-test').addEventListener('click', async (event) => {
  event.stopPropagation();
  const button = $('mic-test');
  const note = $('mic-note');
  button.disabled = true;
  note.classList.remove('bad');
  note.textContent = 'Говорите пять секунд, например: «Шестнадцать шустрых шершней сидели в щели»';
  setTimeout(() => { if (button.disabled) note.textContent = 'Расшифровываю…'; }, 5000);
  try {
    const { peak, text } = await invoke('test_microphone', { device: $('set-microphone').value });
    const loud = Math.round(peak * 100);
    const level = loud < 5 ? 'очень тихо' : loud < 20 ? 'тихо' : loud > 98 ? 'перегруз' : 'громкость в порядке';
    note.textContent = text
      ? `Громкость ${loud}% — ${level}. Расслышано: «${text}»`
      : `Громкость ${loud}% — ${level}. Ничего не расслышано`;
    note.classList.toggle('bad', !text || loud < 5 || loud > 98);
  } catch (error) {
    note.textContent = String(error);
    note.classList.add('bad');
  } finally {
    button.disabled = false;
  }
});

$('unload').addEventListener('click', async () => {
  await invoke('unload_model');
  setTimeout(refresh, 600);
});

/** Подсказывает модели Ollama и выбирает первую, если ещё не выбрана. */
async function loadPolishModels() {
  const polish = state.config.polish;
  const note = $('polish-note');
  $('polish-models').replaceChildren();
  if (polish.mode !== 'ollama') {
    note.textContent = polish.mode === 'openai' ? 'Имя модели, как его называет сервис' : '';
    return;
  }
  try {
    const models = await invoke('ollama_models', { url: polish.url });
    $('polish-models').replaceChildren(...models.map((name) => new Option(name, name)));
    if (models.length === 0) {
      note.textContent = 'В Ollama нет ни одной модели — скачайте, например: ollama pull qwen2.5:7b';
    } else {
      note.textContent = `Нашлось в Ollama: ${models.length}`;
      if (!polish.model) {
        polish.model = models[0];
        $('set-polish-model').value = models[0];
      }
    }
  } catch (error) {
    note.textContent = 'Ollama не отвечает. Установите её с ollama.com и запустите.';
  }
}

/* ── Обновление ── */

async function refresh() {
  const fresh = await invoke('get_state');
  // Словарь, который сейчас правят, из старого состояния не затираем.
  if (state && dictionaryFocused) fresh.config.dictionary = state.config.dictionary;
  state = fresh;
  renderHome();
  renderHistory();
  renderDictionary();
  renderSettings();
}

listen('history', refresh);
// Модель засыпает и просыпается без событий — раз в несколько секунд сверяемся,
// пока окно на экране.
setInterval(() => { if (!document.hidden) refresh(); }, 5000);

refresh().then(() => {
  loadPolishModels();
  if (state.installing) $('install-progress').hidden = false;
});

/* ── Показ без программы ──
   Страница, открытая просто в браузере, показывает выдуманные данные: так её
   можно смотреть и верстать, не собирая программу. */

function demo() {
  const config = {
    hotkey: 'rctrl', language: 'auto', microphone: '', model: 'auto', sounds: true, stream: true, livePreview: true,
    insert: 'paste', trailingSpace: true, voiceCommands: true, keepHistory: true, unloadMinutes: 5,
    autostart: false, onboarded: true,
    polish: { mode: 'off', url: 'http://127.0.0.1:11434', key: '', model: '', instructions: '' },
    dictionary: [{ from: 'постгрес', to: 'PostgreSQL' }, { from: 'Tauri', to: '' }],
  };
  const history = [
    { id: 3, at: new Date().toISOString(), app: 'Telegram', words: 9, seconds: 4, raw: '',
      text: 'Привет! Буду минут через двадцать, начинайте без меня.' },
    { id: 2, at: new Date(Date.now() - 3600e3).toISOString(), app: 'Code', words: 14, seconds: 7, raw: '',
      text: 'Добавь проверку на пустой список перед циклом и верни ошибку, если файл не найден.' },
    { id: 1, at: new Date(Date.now() - 86400e3).toISOString(), app: 'WINWORD', words: 18, seconds: 9, raw: '',
      text: 'Во втором квартале выручка выросла на 12 %, главным образом за счёт новых клиентов из регионов.' },
  ];
  const commands = {
    get_state: () => ({
      version: '0.1.0', config, hasKey: false, installed: true, installing: false, gpu: true, loaded: false,
      paused: false, model: { id: 'turbo', title: 'Точная — large-v3-turbo', megabytes: 1620 },
      models: [
        { id: 'turbo', title: 'Точная — large-v3-turbo, для видеокарты', megabytes: 1620, installed: true },
        { id: 'small', title: 'Быстрая — small, для процессора', megabytes: 488, installed: false },
      ],
      hotkeys: [{ id: 'rctrl', title: 'Правый Ctrl' }, { id: 'ralt', title: 'Правый Alt' }, { id: 'f9', title: 'F9' }],
      hotkeyTitle: 'Правый Ctrl', devices: ['Микрофон (USB Audio)'], history,
      totals: { dictations: 128, words: 4210, seconds: 1890 },
    }),
    save_config: ({ config: next }) => { Object.assign(config, next); },
    capture_hotkey: () => null,
    test_microphone: async () => { await new Promise((r) => setTimeout(r, 1500)); return { peak: 0.42, text: 'Шестнадцать шустрых шершней сидели в щели.' }; },
    cancel_capture: () => null,
    ollama_models: () => ['qwen2.5:7b'],
  };
  return {
    core: { invoke: async (name, args) => structuredClone(commands[name]?.(args) ?? null) },
    event: { listen: async () => () => {} },
  };
}
