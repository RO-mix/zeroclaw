# counterparty-diligence (M2)

Безагентная инфраструктура проверки контрагентов: stdio MCP-сервер (python),
адаптеры к открытым API ФНС, Федресурса, ФНП и санкционным спискам, скоринг
v1 с объяснимостью, sqlite-кэш, досье markdown/json. Скиллы/агенты — тонкие
клиенты этого сервера; логики агента здесь нет.

Источник требований: PRD `counterparty-diligence v2` (разделы 3, 5, 9).

M2 добавляет к ядру M1: реестр МСП (rmsp), банкротства (fedresurs), залоги
ФНП (zalogi — честная деградация, CAPTCHA), цифровой след (crt.sh/whois/RDAP)
и wire-флаги скоринга `msp_confirmed`/`bankruptcy`/`zalog_heavy`/`young_domain`.
Уровень `standard` = M1 + эти четыре источника; `domain` передаётся явно
(`check(domain=...)`, авто-извлечение из материалов — конвейер legalplat).

## Подъём

```bash
cd tools/counterparty-diligence
python -m venv .venv
.venv/Scripts/python -m pip install -r requirements.txt   # Windows
# .venv/bin/python -m pip install -r requirements.txt     # Linux/macOS
```

Python ≥ 3.11. `.venv/` и `data/*.sqlite` не коммитятся (.gitignore).

## Запуск MCP-сервера

```bash
.venv/Scripts/python mcp_server.py        # stdio, для mcp-секции конфига
```

Инструменты (декларации с описаниями — LLM их читает):

- `check(inn=None, name=None, level="quick", domain=None) -> dict` —
  идентификация через ЕГРЮЛ, сбор фактов (стоп-листы ФНС, недобросовестные
  поставщики, санкции; standard/full добавляет реестр МСП, Федресурс,
  залоги ФНП и цифровой след для `domain`), скоринг 0–100 с вердиктом
  green/yellow/red и вкладами флагов.
- `dossier(inn, format="md", domain=None) -> str` — досье по структуре
  SKILL.md v1: сводка/регистрация/владение/МСП/финансы/банкротство/залоги/
  суды/репутация/цифровой след/красные флаги/вывод, покрытие `N/M блоков`,
  дисклеймер «не юридическое заключение», версия модуля и скоринг-модели.
  `format="json"` — сырой dict. Строится на уровне standard.
- `sources_status() -> dict` — доступность источников; санкционные списки
  отдают «disabled (ru-focus)» при выключенном foreign-скрининге.

Скрининг OFAC/EU/UK **опционален и вне российского фокуса**: по умолчанию
`sanctions.foreign.enabled: false` — пайплайн их не трогает. При явном
включении списки надо один раз загрузить в кэш:

```python
from service import DiligenceService
svc = DiligenceService()
print(svc.sanctions.refresh_all())   # OFAC SDN + EU FSD XML + UK FCDO ODS -> sqlite
```

## Сетевая политика: direct-режим для RU

Все запросы к российским источникам (ЕГРЮЛ, pb, npchk) идут **напрямую**:
`sources/base.py: make_session()` по умолчанию ставит `trust_env=False`,
игнорируя ambient `HTTP(S)_PROXY/ALL_PROXY` окружения хоста (симптомы вроде
pb-500 оказались частично следствием прокси-окружения). Прокси включается
только явно: `proxy.enabled: true` в `config.yaml`. `npchk.direct_no_proxy`
сохранён как частное усиление того же поведения. Западные санкционные
списки — по прежней схеме (вне фокуса).

## Источники (ядро M1, без API-ключей)

| Источник | Адаптер | Что даёт |
|---|---|---|
| ЕГРЮЛ ФНС (egrul.nalog.ru) | `sources/egrul.py` | POST → token → `/search-result/{token}`: ОГРН/ИНН/КПП, наименование, адрес, ОКВЭД, руководитель, дата регистрации, статус. PDF-выписка (`/vyp-download/{token}`) — только URL, вне M1 |
| Стоп-листы ФНС (pb.nalog.ru) | `sources/pb_stoplists.py` | POST `search-proc.json` → token → `search-result.json`: массовый адрес/руководитель, недостоверный паспорт, задолженность |
| Недобросовестные поставщики (npchk.nalog.ru) | `sources/npchk.py` | SOAP 1.1 `NdsRequest2` (вручную через requests, без zeep), до 10000 ИНН+КПП+дата за запрос. Схема — по официальным XSD: `<NP INN KPP DT="ДД.ММ.ГГГГ"/>` (атрибуты), namespace `ws.unisoft/FNSNDSCAWS2/Request`; WAF требует браузерный User-Agent. Статусы: 0 действующий, 1 не действующий, 2 зарегистрирован (без даты), 3 КПП≠ИНН, 4+ не найден/прочее |
| Санкции OFAC/EU/UK | `sources/sanctions.py` | OFAC: `sdn.xml`; EU: FSD XML (URL с token из каталога data.europa.eu — голый `/content` геоблокируется с 403 из RU-контура; при отказе — свежий token из каталога или ручное обновление через VPN/внешний хост); UK: ODS от FCDO (`sanctionslist.fcdo.gov.uk`, XML больше не публикуется). Парсинг → sqlite-кэш (`sanctions_entries` + версии). Скрининг: нормализация (lower, кавычки/«», ООО/ОАО/…/LTD/LLC/INC, транслит лат↔кирил), матчинг exact \| fuzzy (все токены ⊆ alias или Dice ≥ 0.8) |
| Реестр МСП (rmsp.nalog.ru) | `sources/rmsp.py` | GET `search-proc.json?query=<ИНН>` — без token-церемоний. Проверено **direct из RU-контура 18.10.2026**: ИНН крупных компаний (Яндекс, Лаборатория Касперского) дают пустую выдачу — это легитимный «не в реестре», а не ошибка. Факты: `msp.in_registry`, `msp.category` (1=микро/2=малое/3=среднее, Постановление Правительства РФ № 265), `msp.inclusion_date`, `msp.active` |
| Федресурс (fedresurs.ru) | `sources/fedresurs.py` | GET `backend/companies?limit=15&offset=0&code=<ИНН>` с **обязательным** `Referer: https://fedresurs.ru/` (иначе отказ). Проверено **direct 18.10.2026**: серверный фильтр `code` выдачу не ограничил (пришло 15 посторонних строк) — клиент сам отбирает точный ИНН. Факты: `bankruptcy.in_registry`, `bankruptcy.is_bankrupt` (маркеры «банкрот/наблюдение/конкурсное/внешнее управление» в `status`), `bankruptcy.status`, `bankruptcy.guid`. Rate-limit ~100 req/час → пауза 4 с (config) |
| Реестр залогов ФНП (reestr-zalogov.ru) | `sources/zalogi.py` | Frontend шлёт POST `/api/search/notary` JSON `{"isPledgorFilter": true, "russianOrganization": {"name","inn","ogrn"}, "limit","offset"}` (схема из sourcemap бандла). Проверено **direct 18.10.2026**: ответ **403 «Не пройдена проверка CAPTCHA»** — честная деградация в «не проверено», как pb. Обход не реализован намеренно; точка расширения — `captcha_token`. Факт при успехе: `zalog.count` (фикстура смоделирована по коду фронта, live-200 не снимался) |
| Цифровой след (crt.sh + whois/RDAP) | `sources/digitalsleuth.py` | whois `.ru/.su/.рф` — сырой сокет к `whois.tcinet.ru:43` (без зависимостей), парсинг `created/paid-till/registrar`; для gTLD — rdap.org. Проверено **direct 18.10.2026**: whois жив (`created: 1997-09-23T09:45:07Z` для yandex.ru), crt.sh в момент проверки отдавал 502/таймаут — best-effort с одним retry, факты `domain.subdomains_count` деградируют в `not_checked`. crt.sh — субдомены по CT-логам, `%.domain&output=json` |

Троттлинг (ЕГРЮЛ/МСП ≥30с, Федресурс ≥4с и пр.) — в `config.yaml`; одиночный retry
ЕГРЮЛ при пустом ответе и crt.sh при 502; при недоступности источника — «не
проверено» в warnings, пайплайн не падает.

## Скоринг v1 (`scoring.py`)

Красные −25 (массовый адрес/руководитель, npchk вне чистых статусов
{0, 2}, санкции, **банкротство Федресурса**), жёлтые −10 (**zalog_heavy** —
залогов больше `zalogi.heavy_threshold`, **young_domain** — домен младше
`domain.young_age_years` при юрлице старше `domain.mature_company_years`),
плюсы +5 (потолок +15; **msp_confirmed** — в реестре МСП и активен).
Итог `max(0, 100 + Σ)` → ≥70 зелёный, 40–69 жёлтый, <40 красный.
Веса/пороги — в `config.yaml`, каждый вклад: `{flag, weight, source,
retrieved_at}`. Красные флаги дисквалификации/разрыва масштаба — M3
(источники ещё не подключены), флаги зарезервированы.

## Ограничения M1–M2

- **pb.nalog.ru (стоп-листы)**: endpoint `search-proc.json` отдаёт HTTP 500
  «Внутренняя ошибка сервера» даже с полной механикой браузера (JSESSIONID,
  User-Agent/X-Requested-With/Referer, page-параметры). Проверено **direct,
  без прокси, из RU-контура 2026-10-17** — причина не в прокси/VPN хоста.
  Флаги стоп-листов честно деградируют в «не проверено» (warning в ответе
  `check`), скоринг не падает.
- **npchk.nalog.ru TLS**: на хостах без цепочки российского УЦ в trust store
  запрос падает с `CERTIFICATE_VERIFY_FAILED` (подтверждено и без прокси —
  это не перехват, а недоверенная цепочка). В `config.yaml`:
  `npchk.direct_no_proxy: true` (обходить proxy-env, default) и
  `npchk.verify_ssl: true` (default). `verify_ssl: false` отключает
  проверку TLS — только осознанно, на свой риск.
- **ЕГРЮЛ**: `/search-result/{token}` отдаёт краткую карточку (ИНН/КПП/ОГРН/
  наименование/дата регистрации/регион; адрес, руководитель, ОКВЭД и статус
  — только когда присутствуют в выдаче). Ответ в cp1251 без charset —
  адаптер декодирует сам. Маппинг ключей сверен живым ответом 2026-10-17
  (`i`=ИНН, `p`=КПП, `o`=ОГРН, `n`=полное имя, `c`=краткое, `r`=дата
  регистрации, `s`=статус, `t`=токен выписки).
- **reestr-zalogov.ru (залоги ФНП)**: поиск — POST `/api/search/notary`
  (схема тела — из sourcemap фронтенда), но бэкенд требует прохождение
  CAPTCHA: проверено **direct из RU-контура 18.10.2026** — ответ 403
  «Не пройдена проверка CAPTCHA». Как и pb: честная деградация раздела
  «Залоги» в «не проверено», скоринг не падает. Обход не реализуем
  намеренно (PRD §9: только легальные каналы).
- **crt.sh**: в момент проверки (18.10.2026, direct из RU) отдавал 502 и
  read timeout — хроническая нестабильность сервиса. Адаптер делает один
  retry, затем деградирует `domain.subdomains_count` в `not_checked`.
- **Федресурс**: серверный фильтр `code` на `/backend/companies` выдачу не
  ограничивает (пришли 15 посторонних компаний на конкретный ИНН) — клиент
  отбирает точный ИНН сам; «не найдено» трактуется как «публикаций нет»
  с confidence `indirect` (индексация могла отставать).
- Нет БФО (bo.nalog.gov.ru — официальный доступ ожидается, решение Ram
  17.10: без обходного парсинга), дисквалифицированных лиц (open data CSV —
  M3), арбитража (внутренний сервис LegalPlat, endpoint уточняется).
- Нет ФССП (капча, API отключён с 2022). Раздел «Суды» — внутренний сервис
  LegalPlat (решения + статусы дел по участнику), доступ к API ожидается;
  kad.arbitr.ru не используем (антибот, заменён внутренним источником).
- Раздел «Финансы» — заглушка до подключения БФО.
- `dossier` требует ИНН (однозначная идентификация); по названию —
  `check(name=...)` с запросом уточнения при тёзках.
- Данные — реестровые публичные сведения; отчёт не является юридическим
  заключением.

## Тесты

```bash
.venv/Scripts/python -m pytest
```

Без живой сети: парсеры и матчинг — на сохранённых ответах в
`tests/fixtures/`, клиенты — на поддельных сессиях, кэш — на tmp sqlite.

## Ориентиры (проверено, 2026-10-17)

Разбор подборки внешнего ИИ (Ram): что берём и что нет.

- **M2+:** `nomenklatura` (OpenSanctions) — заменить самопальный матчинг имён
  при активации санкционного/PEP-скрининга (сильна в русских ФИО/транслите);
  схема **FollowTheMoney** — за основу модели связей (бенефициары/учредители).
- **Продуктовый горизонт LegalPlat:** Aleph (OCCRP) — внутренняя платформа
  поиска по своим архивам. Не часть разведки v1.
- **Не берём:** SpiderFoot/Recon-ng (чужой каркас, западные модули — наш MCP
  уже эта роль под RU-источники); Neo4j/Memgraph/Maltego сейчас (граф — M3
  по потребности, начинать с sqlite + рекурсивных запросов).
- **Граница (Ram):** «Глаз Бога»-глубина (телефоны/авто/недвижимость/родня
  физлиц) — серые агрегаты утечек, не используем.

Полный разбор: `skills/counterparty-diligence/PRD.md` §10 (workspace
legalplat_assistant).
