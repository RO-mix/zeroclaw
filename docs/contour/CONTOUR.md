# CONTOUR.md — операционный контур RAM-RAMII

Контурные правила поверх upstream `AGENTS.md`. Этот файл — только наш, живёт
только в `local/fork-main`, в апстрим не идёт. Апстрим-синхронизации его не
трогают. Точка входа для любой новой сессии в контуре.

## Репозиторий и ветки

- `origin` = zeroclaw-labs/zeroclaw (pull для master; push запрещён).
- `fork` = RO-mix/zeroclaw (push для PR-веток).
- **Каноническая ветка форка: `local/fork-main`** — единственный источник
  прод-билдов (правило скилла `zc-service-ops`). Собрана 13.11.2026
  объединением прод-линии и кодовых фиксов `local/compaction-deploy3`.
- PR-ветки апстрима: от `origin/master`, conventional commits, шаблон PR,
  одна забота на PR. В прод не ставятся.
- **Локальный патч:** `patches/telegram-reply-mention.patch` — после любого
  pull из main переприменять `bash apply-patches.sh`; при конфликте
  регенерировать из рабочего диффа до пересборки.

## Локальный прод (Windows Service)

| Элемент | Значение |
|---|---|
| Сервис | `zeroclaw` (Windows) |
| Бинарь | `C:\Users\Homework\.zeroclaw\zeroclaw.exe` |
| Конфиг (source of truth) | `C:\Users\Homework\.zeroclaw\config.toml` |
| Агенты | `C:\Users\Homework\.zeroclaw\agents\<alias>\workspace\` — регламент из SOUL.md / IDENTITY.md / TOOLS.md / AGENTS.md (не сканировать только AGENTS.md) |
| Трейс | `data/state/runtime-trace.jsonl` (кольцо ~200 событий; история — по `cron list` и `data/cron/jobs.db`) |
| Кроны | только CLI `zeroclaw.exe cron`; jobs.db руками не трогать |
| Gateway | `http://127.0.0.1:42621/health` |

Операции (рестарт, замена exe, сверки): скилл `zc-service-ops`.
Правки конфига: ритуал скилла `zc-config-surgery` (бэкап → Edit →
`config migrate` exit 0 → рестарт → сверка кронов 17 + gateway 200).

## Связанный контур Nota Bene (VPS 94.102.88.185)

- **Канал hc:** herdr-core (`hc`). NB-агент — терминал `cachyos-srv/t17`.
- Письма: `C:/Users/Homework/.zeroclaw/_maintenance/hb-to-notabene-*.md`,
  формат: «ОТ КОНТУРА RAM-RAMII (w1). Тема: … (а)… (б)… (в)… Конец связи.»
  Передача: `hc put cachyos-srv/t17 <файл>` + короткое покрытие `hc send`.
- Канон агентов NB живёт на их стороне (`/opt/zeroclaw-canon`), re-provision
  по их регламенту; живые агенты не трогаем.
- SSH: `cachyos-srv` = 192.168.0.100. Запись `vps-legalplat`
  (193.187.94.160) в ssh-config ошибочна — НЕ NB, не использовать.

## Локализация

- Пользовательский вывод (CLI, инструменты, промпты) — `fl!()` / Fluent,
  без голых литералов. Логи, `tracing::`, panic — английский со стабильными
  error_key. Wiki и внутренние доки контура — русский (как этот файл).

## Правила контура

- Одна компиляция за раз (release ~20–40 мин). Cargo всегда без прокси.
- Git-мутации, рестарты сервиса, push — по визе каждый раз.
- Нововведения: сначала наш билд (`local/fork-main`) и живой тест → потом
  апстрим-PR из отдельной ветки.
- Рестарт сервиса обрывает текущие ходы агентов — предупреждать.
- Subagents: перед любыми shell/fs-операциями `cd` в корень репо.
- Privacy: не коммитить секреты, токены, личные данные; перед отгрузкой
  пакетов на NB — sanitize-check (греп по секретам/именам/paths/chat_id).

## Риск-тиры

- **High:** `crates/zeroclaw-runtime/src/security/`, `zeroclaw-gateway/src/`,
  `zeroclaw-tools/src/`, workflows, любые access-control границы.
- **Medium:** прочие изменения поведения в `crates/*/src/**`.
- **Low:** docs, тесты без смены поведения.

## Память контура

- Постулаты и события: мемлог
  `_bmad-output/planning-artifacts/architecture/architecture-zeroclaw-2026-11-11/.memlog.md`
  + спина ARCHITECTURE-SPINE.md (AD-решения).
- Отложенные работы: `C:/Users/Homework/.zeroclaw/_maintenance/deferred.md`.
- Владелец контура — Ram; решения уровня AD фиксируются в спине.
