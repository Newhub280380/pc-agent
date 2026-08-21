/* Манифест экосистемы. Единственное место, где описан состав модулей:
   отсюда строятся навигация, стартовая сетка хаба и переходы из комнат дома.
   `room` связывает модуль с отделом в 3D-доме (web/house). */
export const MODULES = [
  { id: 'control',   room: 'director',   path: 'control/',
    name: 'CONTROL CENTER',  short: 'Статистика',
    desc: 'выручка, воронка, лиды и финансы одним экраном' },
  { id: 'business',  room: 'sales',      path: 'business/',
    name: 'BUSINESS LOGIC',  short: 'Прайсы и скрипты',
    desc: 'прайс-лист, калькулятор сделки, скрипты и отработка возражений' },
  { id: 'brandkit',  room: 'marketing',  path: 'brandkit/',
    name: 'BRAND KIT',       short: 'Instagram-дизайн',
    desc: 'палитра, типографика и генератор постов/сторис в PNG' },
  { id: 'showcase',  room: 'warehouse',  path: 'showcase/',
    name: '3D SHOWCASE',     short: '3D-витрина',
    desc: 'серебряный шейдер, вся линейка в одной сцене, кадр в PNG' },
  { id: 'portfolio', room: 'admin',      path: 'portfolio/',
    name: 'PORTFOLIO',       short: 'Портфолио',
    desc: 'люксовая презентация бренда и кейсов для партнёров' },
  { id: 'zishel',    room: 'accounting', path: 'zishel/',
    name: 'LANDING',         short: 'Лендинг и заявки',
    desc: 'входящая заявка на прайс с согласием и уходом в WhatsApp' },
  { id: 'card',      room: 'reception',  path: 'card/',
    name: 'BUSINESS CARD',   short: 'Визитка и профиль',
    desc: 'цифровая визитка, шапка профиля Instagram и ссылки для bio' },
  { id: 'fx',        room: 'chill',      path: 'fx/',
    name: 'FX SERIES_01',    short: 'FX-визуал',
    desc: 'рекламный визуал продукта: неон, вращение, запись webm' },
];

export const HUB = { id: 'house', path: 'house/', name: 'MANAGEMENT HOUSE', short: 'Дом' };

/** Путь от текущего модуля к корню web/ — страницы лежат на один уровень глубже. */
export const ROOT = '../';

export const moduleById = id => MODULES.find(m => m.id === id) ?? null;
export const moduleByRoom = room => MODULES.find(m => m.room === room) ?? null;
