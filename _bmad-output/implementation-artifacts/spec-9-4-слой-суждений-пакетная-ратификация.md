---
title: '9-4 Слой суждений с пакетной ратификацией'
type: 'feature'
created: '2026-11-18'
status: 'in-progress'
baseline_commit: 'a4c1a6c6a4'
route: 'dispatch'
review_loop_iteration: 0
context: ['docs/contour/AGENT-MEMORY-CONCEPT.md', 'docs/contour/AGENT-FILE-CANON.md']
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Выводы агента (суждения) нигде не проходят через человека: trend из консолидации извлекается и молча теряется, коррекции оператора — самый сильный сигнал — не захвачены как класс, противоречия канону фиксируются тихим supersede. Нет очереди, нет протокола принятия/отказа, отказ не сохраняется.

**Approach:** Очередь proposals в brain.db с протоколом жизненного цикла propose → review → accept/reject/withdraw → исход эпизодом в архив. Четыре генератора: коррекции оператора (явные фразы + молчаливый канал), противоречия с каноном/графом (reuse conflict.rs), дорогие эпизоды, trend (доводим сигнал из consolidation.rs:30 до записи). Пакетная доставка оператору штатным announce-путём; подпись с тройной привязкой (packet_id + mirror_hash + principal); append-only журнал ratification_receipts с prev-цепочкой; receipt пишется после успеха ВСЕХ записей DEPLOY (транзакция); частичный отказ → остаток pending. Throttle: бюджет proposals/нед; при переполнении trend вырезается первым, коррекции оператора бессрочны. Двойной триггер пакета: календарь (self-review-крон) ИЛИ событие-коррекция. Отказ оператора — сохранённый эпизод, дороже принятия.

## Boundaries & Constraints

**Always:** всё за флагом конфига (дефолт выкл, байт-в-байт); только ADD-миграции brain.db; user-facing тексты через fl!(); подпись принимается только от principal из конфига; MCP-инструментов перехода состояний нет (кислотный тест HC); операторский канал (Interactive) вне гейта.

**Never:** self-ратификация агентом никаким путём; MCP-сервер НЕ входит в этот патч (история 9-4б); журнал file-events фиксирует только факт вызова (путь, метка, turn-id) — хранение diff по-прежнему запрещено ADR-016; не менять каналы/оркестратор кроме точки перехвата ответа оператора; апстрим-PR.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| генератор trend | consolidation вернула trend, фича вкл. | запись proposal (kind=trend, source_refs на эпизод) | trend=None — no-op |
| генератор коррекций (явные) | текст оператора матчит фразы коррекции в turn'е | proposal (kind=correction, priority highest, бессрочно) | не матчит — no-op |
| событие-коррекция → пакет | proposal kind=correction создан | немедленная сборка пакета (не ждёт календаря) | доставка недоступна → остаётся pending, retry по расписанию |
| календарный пакет | self-review-крон / таймер | пакет из pending proposals старше порога | пустой пакет — не доставлять |
| подпись оператора | «утверждаю» от principal на packet_id | DEPLOY транзакционно: записи + receipt (prev-chain), остаток pending | не principal → отказ с reason; receipt не пишется |
| отказ оператора | «отклоняю» + комментарий | proposal → rejected-эпизод в архиве (затухает-с-аудитом, исключён из retention) | — |
| throttle | proposals > бюджет/нед | приоритет: коррекции сохраняются, trend вырезается первым | счётчик в отчёте пакета |
| фича выключена | любой вызов | поведение байт-в-байт (trend теряется как раньше) | — |

</frozen-after-approval>

## Решения оператора (18.11, все «по рекомендации»)

- **OQ-1 → Б:** MCP-поверхность — отдельной историей 9-4б (в этом патче только очередь + ратификация; таблицы спроектированы так, чтобы 9-4б их переиспользовал).
- **OQ-2 → Б:** principal ратификатора — новый ключ `[memory.ratification] principal` + `principal_transport = "telegram"`; не зависеть от `owner_chat_id` канала. Allowed-senders канала уже ограничивают круг.
- **OQ-3 → А (урезанный):** узкий журнал file-events в brain.db: факт tool-call file_write в воркспейс агента (путь, метка времени, turn-id). БЕЗ хранения содержимого diff — ADR-016 в целом сохранён, поправка зафиксирована в Implementation Notes.
- **OQ-4 → А:** текстовая подпись «утверждаю [N]» / «отклоняю [N]: причина» — работает во всех каналах без платформенных кнопок.

## Code Map

- `crates/zeroclaw-memory/src/consolidation.rs:28-30` — `ConsolidationResult.trend` извлекается LLM и нигде не читается (подтверждено grep) — доводим до записи в proposals.
- `crates/zeroclaw-memory/src/conflict.rs` — check_and_resolve_conflicts (:7), find_text_conflicts (:87), mark_superseded (:42): генератор «противоречия» reuse + журнал конфликта как proposal.
- `crates/zeroclaw-memory/src/sqlite.rs` — DDL-точка: новые таблицы `proposals` (kind, payload, status, priority, created/resolved, packet_id) + `proposal_refs` (source refs, таблица не json) + `ratification_receipts` (append-only: packet_id, mirror_hash, principal, prev_hash, created). Прецеденты ADD-only: memory_refs :368, self_change_receipts :384.
- `crates/zeroclaw-memory/src/receipts.rs` — соседство с self_change_receipts (9-3); ratification-receipts — отдельная таблица, не путать.
- `crates/zeroclaw-runtime/src/cron/scheduler.rs:1533-1607` — доставка: DeliveryConfig announce → DELIVERY_FN → orchestrator::deliver_announcement (mod.rs:18226); двойной триггер = существующий scheduler + немедленный вызов сборки пакета.
- `crates/zeroclaw-channels/src/orchestrator/mod.rs:8422-9448` — ingress канала, IngressContext.sender; точка перехвата ответа оператора на пакет (перед process_channel_message_body).
- `crates/zeroclaw-config/src/schema.rs:17388` — owner_chat_id в TelegramConfig (ориентир соседства; principal берём из нового `[memory.ratification]`, не отсюда).
- `crates/zeroclaw-runtime/src/agent/tool_receipts.rs:15-186` — HMAC tool receipts (turn-local); узкий журнал file-events — новая таблица в brain.db (факт вызова, не diff).
- `crates/zeroclaw-runtime/src/daemon/mod.rs:1313-1355` — spawn_component_supervisor: новый компонент ratification-worker (периодический прогон генераторов + сборка пакетов), прецедент hygiene state-file троттлинга (hygiene.rs:13-57).
- Продовые вызовы уборки (не трогать): daemon/mod.rs:1066, :2707. `memory_stats` CLI — расширить счётчиками proposals (паттерн 9-3).
- Тесты: tempdir SqliteMemory (memory), scheduler delivery-fn захват (scheduler.rs:4119), ingress тест (orchestrator :20695) — оркестратор-тесты тяжёлые; перехват подписи покрыть юнитом парсера + интеграцией на уровне runtime.

## Tasks & Acceptance

**Execution:**
- [ ] `crates/zeroclaw-config/src/schema.rs` -- `[memory.ratification]` (nested): `enabled: bool` (false), `principal: String` (""), `principal_transport: String` ("telegram"), `packet_trigger_days: u32` (7), `weekly_budget: u32` (20), `min_packet: u32` (1) -- флаги без смены дефолтов.
- [ ] `crates/zeroclaw-memory/src/proposals.rs` (новый) -- таблицы proposals/proposal_refs/ratification_receipts (DDL в sqlite.rs, ADD-only) + операции: propose (идемпотентно по source-hash), пакетная выборка, apply_signed (транзакция: accepted/rejected-эпизоды + receipt с prev-chain), throttle-отчёт. Чистая логика над rusqlite, юнит-тестируемо.
- [ ] `crates/zeroclaw-memory/src/consolidation.rs` -- trend (:28-30) при включённой фиче пишется в proposals (kind=trend, source_refs на эпизод), раньше — молча теряется.
- [ ] `crates/zeroclaw-memory/src/conflict.rs` -- факт разрешённого противоречия (supersede) журналируется как proposal (kind=contradiction) при включённой фиче.
- [ ] `crates/zeroclaw-runtime/src/agent/ratification.rs` (новый) -- генератор коррекций (парсер фраз + file-events журнал), сборщик пакета (mirror = canonical JSON proposals), разбор текстовой подписи оператора, вызов apply_signed.
- [ ] `crates/zeroclaw-runtime/src/daemon/mod.rs` -- компонент ratification-worker под spawn_component_supervisor: календарный триггер (packet_trigger_days) + событийный канал (correction → немедленная сборка).
- [ ] `crates/zeroclaw-channels/src/orchestrator/mod.rs` -- точка перехвата: сообщение от principal, матчинг подписи на активный пакет → ratification-обработка вместо обычного turn'а (за флагом).
- [ ] `src/memory/cli.rs` -- `memory stats`: счётчики proposals по статусам.
- [ ] Тесты: proposals (жизненный цикл, идемпотентность, prev-chain, транзакционность), trend→proposals, коррекции (явные + file-events), подпись (principal/non-principal, частичный отказ → pending), throttle, фича-выкл байт-в-байт.

**Acceptance Criteria:**
- Given фича выключена (дефолт), when консолидация/конфликты/сообщения, then поведение байт-в-байт текущему (trend теряется как раньше).
- Given enabled и trend извлечён, when фазовый прогон, then proposal kind=trend с source_refs; повторный прогон не дублирует (идемпотентность по source-hash).
- Given correction-detected, when генератор, then proposal priority=highest и немедленная попытка сборки пакета (двойной триггер).
- Given пакет доставлен и «утверждаю» от principal, when apply_signed, then ВСЕ записи и receipt в одной транзакции; receipt.prev_hash — хвост цепочки; ответ не-principal → отказ, receipt не пишется.
- Given частичный отказ («отклоняю 2»), when apply_signed, then пункт 2 → rejected-эпизод с комментарием, остаток pending.
- Given throttle переполнен, when сборка, then коррекции сохранены, trend отброшен первым, факт отбора в отчёте пакета.
- Given proposals старше N дней без подписи, when календарный триггер, then затухание-с-аудитом через механизм 9-2 (исключены из retention-очистки до resolved).
- Given `cargo test -p zeroclaw-memory -p zeroclaw-runtime` (затронутые модули), when прогон, then зелёное.

## Implementation Notes

Расследование (agent-187, 18.11). Ключевые факты: DELIVERY_FN — процесс-глобальный OnceLock, один announce-путь на процесс (достаточно для пакетов). trend реально мёртв (grep подтвердил). MCP-сервера в репо нет — основание OQ-1. ADR-016 запрещает durable diff capture — основание OQ-3. Self-review-крон юзера — обычный agent-job в jobs.db; календарный триггер 9-4 независим (свой worker). Неприкосновенные зоны: crates/zeroclaw-eval/runner.rs, budget.rs, narrative.rs (грязные чужие правки в дереве).

## Spec Change Log

## Review Triage Log

## Verification

**Commands:**
- `cargo test -p zeroclaw-memory -p zeroclaw-config` -- expected: зелёное с новыми тестами proposals.
- `cargo test -p zeroclaw-runtime ratification proposals` -- expected: зелёное.
- `cargo check -p zeroclaw-channels --tests` -- expected: без ошибок (точка перехвата).
- `cargo fmt --all -- --check` -- expected: чисто.

**Manual checks (if no CLI):**
- Включённый пилот (отдельной визой): прогон генератора на тренировочном turn'е → пакет ушёл оператору; «утверждаю» → receipt в ratification_receipts, prev-chain сходится.
