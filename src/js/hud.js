// Индикатор записи. Состояние приходит из Rust событием `hud`, громкость —
// `hud:level`, уже расслышанный текст — `hud:partial`.

const { listen } = window.__TAURI__.event;

const stage = document.getElementById('stage');
const bars = document.getElementById('bars');
const label = document.getElementById('label');
const partial = document.getElementById('partial');

const BARS = 22;
const levels = new Array(BARS).fill(0);
for (let i = 0; i < BARS; i += 1) bars.append(document.createElement('i'));

const LABELS = {
  listening: '',
  locked: 'без рук — клавиша или Esc',
  transcribing: 'Расшифровываю',
  polishing: 'Причёсываю текст',
  done: 'Готово',
};

function showPartial(text) {
  // В окошке две строки, показываем хвост: человеку важно, что расслышано
  // только что, а не начало фразы.
  const tail = text.length > 110 ? `…${text.slice(-110).replace(/^\S*\s/, '')}` : text;
  partial.textContent = tail;
}

listen('hud', ({ payload }) => {
  const { state, text } = payload;
  stage.dataset.state = state;
  label.textContent = text || LABELS[state] || '';
  if (state === 'listening' || state === 'locked') {
    if (state === 'listening') {
      levels.fill(0);
      showPartial('');
    }
  } else if (state !== 'transcribing' && state !== 'polishing') {
    showPartial('');
  }
});

listen('hud:level', ({ payload }) => {
  // Громкость речи редко выше трети шкалы; корень поднимает тихое, чтобы
  // полоски отвечали и на спокойный голос.
  const level = Math.min(1, Math.sqrt(Math.max(0, payload)) * 1.25);
  levels.push(level);
  levels.shift();
  const nodes = bars.children;
  for (let i = 0; i < BARS; i += 1) {
    nodes[i].style.height = `${3 + levels[i] * 23}px`;
  }
});

listen('hud:partial', ({ payload }) => {
  const state = stage.dataset.state;
  if (state === 'listening' || state === 'locked') showPartial(payload);
});
