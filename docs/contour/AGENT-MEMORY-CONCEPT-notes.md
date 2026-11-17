# Заметки кода: модули памяти zeroclaw (полное чтение, 17.11)

Источники: crates/zeroclaw-memory/src, crates/zeroclaw-runtime/src/agent. Все факты проверены чтением.

## memory_strategy.rs (65 стр.) — ПОЛНОСТЬЮ
- DefaultMemoryStrategy: consolidate_turn → consolidation::consolidate_turn; run_governance → hygiene::run_if_due. Тонкий делегатор, точка входа рантайма в память.

## policy_gate.rs (25 стр.) — ПОЛНОСТЬЮ
- validate_store: policy.validate_store(namespace, category) + лимиты count по namespace и category. Защита объёма, не смысла.

## hygiene.rs (1117 стр.) — поток полностью
- run_if_due: раз в 12ч (HYGIENE_INTERVAL_HOURS), state в state/memory_hygiene_state.json.
- Действия: archive memory/session files (archive_after_days) → purge архивов (purge_after_days) → prune_category_rows по retention (conversation/daily/core, policy-override) → compact_budget_rows → prune_audit_entries.
- **Ключевое: hygiene УДАЛЯЕет строки физически** (DELETE по retention). Core тоже режется (с флагом). Это destructive retention, НЕ fade. Противоречит п.5 концепции (затухание с аудитом) — утечка: retention-дней и аудит-находимость несовместимы в текущем коде. Audit prune — отдельно.
- best-effort: ошибки логируются, не падает.

## consolidation.rs (1655 стр.) — поток полностью
- consolidate_turn: LLM (ProviderDispatch.chat_with_system) с промптом-экстрактором. Турнат truncated 4000 симв.
- Результат: history_entry → Daily (daily_<date>_<uuid>); memory_update → Core (core_<uuid>) с importance (эвристика); facts[] (опц., max 5/turn, consolidation_extract_facts) → Core отдельно; trend — возвращается, НО никуда не пишется (видно по коду: trend не сохраняется! — проверить, но в consolidate_turn после parse используются history_entry/memory_update/facts/kind; trend — нет).
- Защита записи Core: policy_gate → dedup (Insert/Reject/Merge) → conflict::check_and_resolve_conflicts → store_with_options → supersede(losers) — **supersede = обратимый soft-hide** (conflict_supersede_enabled, default on).
- store_typed_facts: тот же путь защиты для атомных фактов.
- **Ключевое:** всё долговременное падает в Core как плоский поток UUID-записей. Нет типа «суждение», нет версионирования смысла, нет связи с графом, trend теряется. Консолидация пишет НАПРЯМУЮ (без очереди/ратификации) — агент сам себе архитектор памяти.

## knowledge_graph.rs (1124 стр.) — поток полностью
- SQLite LPG: nodes(id,type,title,content,tags,created,updated,source_project) + edges(from,to,relation, PK тройка) + FTS5 с триггерами. WAL.
- 8 типов узлов (pattern/decision/lesson/expert/technology/client/contact/interaction), 8 связей (uses/replaces/extends/authored_by/applies_to/manages_client/contact_of/interacted_with).
- add_node: жёсткий max_nodes — при превышении ошибка (без эвикции). add_edge: проверка существования. query_by_similarity: только FTS-rank (BM25-like), БЕЗ графового скоринга, БЕЗ обхода. query_by_tags: полный скан фильтром. get_node по id.
- **Нет:** provenance на эпизоды/документы, весов рёбер, temporal-полей, decay, мягкого удаления (DELETE CASCADE), центральности, авто-наполнения. max_nodes — потолок без политики.
- knowledge_graph_pg: та же модель на Postgres + векторная близость. Не используем.

## memory_inject.rs (2041 стр.) — поток полностью
- Единый пайплайн инжекта на ход: resolve_inject_policy(TurnOrigin): SubTurn→Skip; Cron/Daemon→Inject exclude_conversation; Interactive/Channel/AgentDirect→Inject.
- Пайплайн: recall (per session, key-dedup) → decay-arm ИЛИ rerank-arm (rerank_enabled: over-fetch pool, blend recency/importance, MMR, threshold) → skip-set (autosave keys, *_history, [IMAGE:, <tool_result) → бюджеты (max_entries 4, entry 800 chars, total 4000).
- **Нет always-load:** pinned/core не гарантированы в контексте — всё через relevance. Core-category exempt только от decay-скоринга, не от отбора.
- Конфиг из MemoryConfig: min_relevance_score, rerank_strategy (mmr/none), weights, recall_date_stamp.

## Сводные выводы для концепта
1. Три подсистемы (episodic core-поток / граф / инжект) существуют, но НЕ СЦЕПЛЕНЫ: консолидация не пишет в граф; инжект не читает граф; граф пуст.
2. Уничтожение в hygiene противоречит обратимости (п.5) — реальная точка конфликта.
3. supersede в консолидации — готовый механизм обратимого скрытия (забираем для fade).
4. trend из консолидации теряется — готовый недоиспользованный сигнал для уровня «суждения».
5. policy_gate — объёмные лимиты; полномочий «кто пишет какой категории» нет (нет operation-authorization).
6. MCP-поверхность памяти = 5 инструментов (store/recall/forget/export/purge) — forget/purge доступны агенту напрямую: агент может уничтожать свою память (контр-пункту 6 концепции: цена изменения).

## Дополнение (диалог, 17.11)
- В7-расследование: `auto_capture` (schema.rs:9813) и `suggest_on_query` (schema.rs:9816) — фантомы конфига, потребителей 0 во всех crates/src. Knowledge tool живёт только в явных вызовах агента (tools/mod.rs:3756–3775). Граф «включён и пуст» = ручной инструмент без автопилота; апстрим декларирует автоподпитку без реализации. Д1 = стройка, не ремонт.
- `trend` (consolidation.rs:30): подтверждено — парсится и нигде не используется (write-путь и тесты).
