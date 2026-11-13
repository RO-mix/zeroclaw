# ДРАФТ: Сравнительная матрица — кандидат knowledge graph-контура

Статус: **ДРАФТ / предварительная оценка**. Направление от Ram 13.11.2026: «взять лучшее из
двух миров» — форк grafeo-notebook с закрытием недочётов либо гибрид с концепциями gbrain.

Источники: отчёты субагентов agent-156/157/158 (сессионные tasks/*/output.log),
BMad-документация grafeo-notebook (cachyos-srv:/mnt/data/prog/grafeo-notebook/docs).

## Матрица

| Критерий | grafeo-notebook (база форка) | gbrain (донор концепций) | Гибрид (целевой кандидат) |
|---|---|---|---|
| Язык/стек | Rust, GrafeoDB embedded | TypeScript/Bun, PGLite/pgvector | Rust; концепции gbrain, ядро своё |
| Retrieval-ядро | CPU-graph-first ask(): PPR/PageRank, edge-weighted PCST, RRF, structural rerank (ADR-0030) | 4 recall-арма, page-grain RRF k=60, re-score 0.7/0.3, rerank-протокол | Ядро grafeo-notebook + механики gbrain (см. ниже) |
| Граф | L0: LPG+RDF через Grafeo, typed edges, quality scores, **sentence-provenance** | typed edges + provenance + auto-link без LLM; BFS-обход | Оба подхода совместимы; сильнейшая сторона обоих — typed traversal, не retrieval |
| Чанкинг/ингест | Есть пайплайн; **время инжеста большое (беспокойство Ram, не замерено)** | Чанкер 300 слов/50 overlap, CJK-aware, потолки 6000c/2000t, contextual retrieval, версионирование чанкера | Портировать чанкер gbrain + его инкрементальность/сталeness-механики (signature, embedded_text_hash, backfill) |
| Векторный поиск | **brute-force (HNSW заявлен в PRD, нет в коде)** | HNSW (pgvector), итеративный scan, per-page max-pool | HNSW через Grafeo (умеет) — закрыть недочёт PRD |
| Keyword/FTS | RU-BM25 (свой) | Postgres FTS + CJK-сплиттер, веса A/C | RU-BM25 свой + весовые схемы gbrain |
| Embeddings | Есть (модель не верифицирована в отчёте) | Мультитенантные провайдеры, Voyage-4 1024d default, keyword-only деградация (fail-open, `degraded` meta) | Провайдер-абстракция + явный degraded-режим как первоклассный |
| Rerank | structural rerank | cross-encoder, head-only (topNIn), doc-cap 1400 tok, fail-open в RRF-порядок | Протокол gbrain (дешёвый, fail-open) — адаптировать |
| Синтез/ответ | evidence trees, ground truth | `think`: JSON-контракт, цитаты [slug#row], gaps[] = поле промпта | Промпт-контракт gbrain (цитаты/конфликты/gaps) поверх своего синтеза |
| Ночной цикл | cron в bridge, backfill-прогоны | dream cycle: фазовая maintenance (lint→consolidate→embed-backfill→drain) | Фазовая декомпозициz gbrain как образец для своего цикла |
| NLP | «тяжёлый NLP»: OpenIE, NER (gRPC-сервис), RU-покрытие | LLM-извлечение атомов (extract_atoms) с budget-gate | Свой NLP остаётся — преимущество перед gbrain |
| Бенчмарки | benchmark_ask_accuracy.py, ablation (эпик 61 done) | BrainBench + LongMemEval 95.5% recall@5 (внешний harness) | Совместить: свои ablation + метрики gbrain как целевые |
| BMad-контур | Полный (PRD, эпики 58-68, стори-спеки) — готовая управляемость | Нет | Сохранить BMad-ведение форка |
| Лицензия | Свой проект (Ram) | MIT (концепции не копируют код) | Без конфликтов |
| Интеграция с ZeroClaw | bridge UDS заявлен в architecture | MCP-сервер | MCP-инструмент для агентов ZeroClaw — естественная точка входа |
| Риски | Brute-force вектор; устаревшая architecture.md; статус-файл отстаёт; Kuzu-следы; инжест медленный | 486 МБ чужого продукта, ежедневный дрейф — форк неподъёмен | Объём работ: порт чанкера + HNSW + rerank-протокол + промпт-контракт = оценочно 1 эпик |

## Недочёты grafeo-notebook (закрыть в форке)

1. HNSW: заявлен в PRD, реализован brute-force → включить Grafeo HNSW, формально закрыть ADR.
2. Время инжеста (замерить! сейчас — беспокойство без цифр) → портировать чанкер/сталeness/инкрементальность gbrain.
3. architecture.md устарела → сторя 68-7 из backlog.
4. sprint-status отстаёт от git → гигиена спринта.
5. Kuzu dual-runtime → решить: оживить или похоронить явно.
6. Degraded-режим поиска без эмбеддингов → как у gbrain (explicit meta, fail-open).

## Открытые вопросы к сессии визирования

1. Замер инжеста: на каком корпусе и что считать нормой (gbrain: soft-delete, re-walk по версии чанкера)?
2. Форк оформить как новый эпик в BMad-контуре grafeo-notebook или как отдельный проект?
3. Точка внедрения в ZeroClaw: MCP-инструмент поверх форка — сразу или после эпика?
4. Связь с апстрим-линией: пнуть #11053 (ревизия висит с 29.09) — коммент со ссылкой на замеры?
5. Пилотный сценарий связки: legalplat (связи документов) или хроника Демиурга?

## Рекомендация (предварительная)

**Форк grafeo-notebook как база, gbrain — донор проверенных механик** (чанкер,
page-grain RRF, rerank-протокол, degraded-режим, промпт-контракт think, фазовый
цикл). Портировать ядро механик, не код. Первый эпик форка: HNSW + замер/фикс
инжеста. Не начинать до визы Ram и замера инжеста.
