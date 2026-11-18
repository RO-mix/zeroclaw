# Глобальный обзор: архитектуры памяти агентов и алгоритмы dreaming/consolidation (конец 2026)

> Исследовательский отчёт. Дата среза: ноябрь 2026. OpenClaw memory-core dreaming родитель покрыл отдельно и здесь не рассматривается (одна строка: 3-фазный Light/REM/Deep + 6-сигнальный скоринг — один из самых детально описанных production-механизмов dreaming в индустрии).
>
> Метод: веб-поиск по целевым запросам + чтение первоисточников (arXiv, официальные блоги Letta/Anthropic/MemTensor, анализ утечки исходников Claude Code 2.1.88, тех-отчёт MemOS по исходникам). Предпочтение — первоисточникам; вторичные обзоры помечены.

## Как читать

Для каждой системы — карточка по осям: Trigger/cadence · Pre-model gating · Роль модели · Write-path gating · Provenance/audit · Forgetting/decay · Eval · Failure modes · Advanced vs marketing · Передаваемые механизмы.

---

## 1. Letta / MemGPT — sleep-time compute, блоки, дивергенция

Источники: arXiv:2504.13171 (Lin, Snell, Wang, Packer, Wooders, Stoica, Gonzalez — Letta + UC Berkeley), letta-ai/sleep-time-compute (код воспроизведения), блог Letta «Why memory isn't a plugin» (апрель 2026), Letta 0.7.0 sleep-time agents, обзор seekvana (вторичный).

**Архитектура.** Три уровня памяти по аналогии с ОС: **Core memory** — несколько именованных блоков (human, persona), всегда в контексте, редактируются моделью через tool calls (`core_memory_append` / `core_memory_replace`); **Recall** — полная история диалога, точный поиск; **Archival** — векторное долговременное хранилище (`archival_memory_insert` / `archival_memory_search`). Агент сам решает, что сохранять. На DMR (Deep Memory Retrieval): 32.1% → 92.5% (GPT-4, long-conversation fact recall).

- **Trigger/cadence**: два режима. (а) Внутри сессии — write-решения принимаются на каждом ходу моделью (event-driven). (б) Sleep-time: heartbeat/idle-проходы — фоновый агент получает ход без user message, либо consolidation при compaction контекста; в исследовании — офлайн-проход «на ночь».
- **Pre-model gating**: минимальная. Блоки имеют жёсткие лимиты размера; в sleep-time агент получает «сырые» разговоры целиком, отбор делает сама модель. Детерминированных gate'ов почти нет — сознательный дизайн-выбор, а не oversight.
- **Роль модели**: возвращает операции (tool calls), применяет runtime (Letta server). В sleep-time compute — сильная модель пишет память для слабой/быстрой основной.
- **Write-path gating**: пишет сам агент; человек — через ADE (графический отладчик) или API. Human-in-the-loop в write-path по умолчанию отсутствует.
- **Provenance/audit**: слабый. Блоки — свободный текст; история правок версионируется в Letta Code через git-backed filesystem (MemFS), но source-ссылки «эта запись из этого сообщения» не являются first-class.
- **Forgetting/decay**: нет формальной модели. Забывание = вытеснение из блока при append/replace и недоступность архива при плохом поиске.
- **Eval**: DMR 92.5% vs 32.1% fixed-context; sleep-time paper: −~5x test-time compute при той же точности на Stateful GSM-Symbolic/AIME, до +13%/+18% точности при масштабировании sleep compute; Multi-Query — −2.5x стоимость/запрос. Ключевой собственный вывод: эффективность sleep compute сильно коррелирует с предсказуемостью будущих запросов.
- **Failure modes (документированные)**: (1) **memory divergence** — sleep-агент постепенно «расслаивается» с поведением основного: консолидированные заметки перестают соответствовать тому, как основной агент реально работает, и могут сделать его хуже (главный риск sleep-time agents по самой Letta; см. обсуждение в hermes-agent-evolution #2358); (2) агент должен *решить* сохранить факт — если не решил, факт потерян при скролле контекста; (3) heavy turns: ~25k токенов контекста на ход даже «пустому» агенту (обзор agentsystems.blog, ноябрь 2026); (4) память целиком зависит от judgment модели.
- **Advanced vs marketing**: genuinely advanced — самая старая и воспроизводимая линия (код открыт, бенчмарки воспроизводимы), sleep-time compute имеет формальную оценку. Marketing-слой: 5x/13%/18% — цифры вендора на собственных модифицированных бенчмарках, независимой репликации мало.
- **«Memory Models» 2026**: Letta переупаковывает framing — агент = «модель памяти» (системный промпт + блоки + инструменты), harness обязан сам управлять контекстом («memory isn't a plugin»); Letta Code проецирует память в git-backed файловую систему, которую фоновые memory-субагенты правят конкурентно. Движение от «память как плагин» к «память как часть ОС агента».
- **Передаваемые механизмы**: именованные редактируемые блоки с лимитом размера; отдельный sleep-агент с эксклюзивным write-доступом; правило «сильная модель консолидирует для слабой»; явная проверка «улучшила ли консолидация поведение» (antidivergence-тест).

---

## 2. Zep / Graphiti — bi-temporal knowledge graph

Источники: arXiv:2501.13956 (Rasmussen et al.), getzep/graphiti, Neo4j blog, независимые обзоры (digitalapplied, callsphere, sentra).

**Архитектура.** Эпизоды (message / text / JSON) инкрементально превращаются LLM-экстрактором в entities и relations графа. Каждое ребро имеет **два временных измерения**: valid time (когда факт был истинен в мире) и transaction/ingestion time (когда система его узнала). При противоречии старое ребро **инвалидируется, а не удаляется** (non-destructive invalidation: `invalidated_at`, `valid_to`). Результаты: DMR 94.8% vs MemGPT 93.4%; в независимом замере с GPT-4o на LongMemEval: Zep 63.8% vs Mem0 49.0%.

- **Trigger/cadence**: строго **per-episode, streaming** — каждое сообщение сразу после события (delta-based, без отдельной фазы «сна»). Consolidation происходит *на записи*.
- **Pre-model gating**: детерминированный проход перед моделью: episode буферизуется, entity-resolution по косинусной близости к существующим узлам (dedup-окно), затем LLM извлекает triples и решает add/merge/invalidate. Сырой текст в модель не идёт — идёт уже кандидат-связка.
- **Роль модели**: возвращает структурированные операции (entity nodes, relations, temporal resolution, contradiction decision), применяет движок Graphiti.
- **Write-path gating**: пишет ingestion-пайплайн (LLM-driven), не сам агент. Human-in-the-loop нет; внешний gate — соответствие схеме/онтологии (custom entity types).
- **Provenance/audit**: сильная сторона. Эпизод — first-class provenance-узел; у каждого факта source episode, created_at, invalidated_at, valid_from/to. Bi-temporal запросы: «что мы знали на дату X о состоянии на дату Y».
- **Forgetting/decay**: нет декей-по-времени; «забывание» = факт уходит из working context, но остаётся в графе (append-only). Управление размером — операционное, не когнитивное.
- **Eval**: DMR 94.8% (SOTA на момент публикации), LongMemEval (независимый замер GPT-4o: 63.8%).
- **Failure modes**: стоимость LLM на каждый эпизод; schema drift извлечённых сущностей; граф-БД как эксплуатационная нагрузка; качество contradiction-resolution целиком на judgment одного LLM-вызова (ошибка invalidation необратимо портит temporal-запросы); экстракция теряет информацию против raw dialogue (зафиксировано в сравнительных исследованиях, arXiv:2604.01707).
- **Advanced vs marketing**: genuinely advanced в temporal-модели (bi-temporal + non-destructive invalidation — редкая полнота). Критика: «Zep — это AI-дата-слой, а не мозг»; нет ни dreaming, ни эпистемической политики — система помнит, но не «учится».
- **Передаваемые механизмы**: bi-temporal метки на каждый факт; non-destructive supersession (invalidate, не overwrite); episode как first-class provenance; детерминированный entity-resolution перед LLM.

---

## 3. Mem0 / Mem0g

Источники: arXiv:2504.19413 (ECAI 2025, Chhikara et al.), docs.mem0.ai (memory-evaluation), независимые замеры (digitalapplied, callsphere, arXiv:2604.01707, prehistoric/paper).

**Архитектура.** Двухфазный пайплайн на сообщение: **Extract** (LLM извлекает candidate facts из новой пары сообщений + небольшого history-контекста) → **Update** (LLM сравнивает кандидатов с retrieved существующими и выдаёт операции ADD / UPDATE / DELETE). Варианты: Mem0 (вектор), Mem0g (+Neo4j граф, entities/relations извлекаются параллельно и хранятся отдельно).

- **Trigger/cadence**: per-message, синхронно при `add()` (в новых версиях — опциональный async + временная метка + декей, майское обновление 2026). Отдельного offline-consolidation нет: consolidate = встроенный шаг Update.
- **Pre-model gating**: да — перед Update-LLM идёт vector-retrieval top-k существующих памятей (детерминированный проход), кандидаты дедуплицируются эмбеддингами; текст может предварительно скориться на «стоит ли извлекать» (salience prompt / extract on turn end).
- **Роль модели**: возвращает строгий JSON операций (ADD/UPDATE/DELETE с памятью-получателем); применяет код SDK. Никакого свободного прозаического консолидирования.
- **Write-path gating**: пишет сервис по API, человек — через delete API/dashboard. DELETE реализовано как операция LLM, а не как право владельца данных.
- **Provenance/audit**: минимальна. Факт = строка + embedding + метаданные; source-ссылка на конкретное сообщение нет first-class; истории supersession нет (UPDATE перезаписывает).
- **Forgetting/decay**: в базовой версии — нет; забывание только через DELETE. В обновлениях 2026 заявлены временные метки и memory decay по убыванию полезности (механика не опубликована детально).
- **Eval**: self-reported: LoCoMo +26% LLM-as-judge vs OpenAI memory; LongMemEval 92.5–94.4 self-reported при ~7k токенах на retrieval. Независимые замеры сильно ниже: 49.0 на LongMemEval (GPT-4o, shared stack) — разрыв вероятно связкой «свои стор/embedder против shared-стека»; один разбор прямо фиксирует: экстракция сохраняет gold-fact, но single-vector retrieval ранжирует его последним — «genuine limitation of dense-only memory». Mem0g даёт лишь ~+2% к общему скору.
- **Failure modes**: (1) dense-only retrieval проваливает ранжирование редких фактов; (2) UPDATE перезаписывает без истории → ошибка экстрактора необратима; (3) скрытая стоимость: 2 LLM-вызова на сообщение; (4) self-reported цифры не воспроизводятся независимо (92.5 vs 49).
- **Advanced vs marketing**: честный минимализм («drop-in»), но consolidation — простейший из рассмотренных: нет offline-фазы, нет provenance, нет human gate. Advanced: production-зрелость, 24+ бэкенда, граф-вариант. Marketing: бенчмарки против собственного стека.
- **Передаваемые механизмы**: строгий JSON операций с памятью-получателем (не свободный текст); детерминированный retrieval перед update-решением; измерение «токенов на retrieval-вызов» как метрика качества памяти.

---

## 4. A-MEM и преемники (Zettelkasten-память)

Источники: arXiv:2502.12110 (Xu et al., NeurIPS 2025), репозитории agiresearch/A-mem / WujiangXhu/A-mem, обзоры (effloow, local-ai-zone, thecontinuitylayer), преемники: MEMI (ICLR 2026), SimpleMem (arXiv:2601.02553).

**Архитектура A-MEM.** Каждое событие → **атомарная заметка** (content, keywords, tags, contextual description, links). Три операции — **Note Construction**, **Link Generation** (bidirectional ссылки на близкие старые), **Memory Evolution** (при приходе новой заметки система может пересмотреть описания/ссылки затронутых старых — по порогу похожести, по умолчанию evo_threshold=100 в публикации).

- **Trigger/cadence**: write-time, per-event; offline-фазы нет: организация памяти происходит на записи, эволюция — каскадно при новых событиях.
- **Pre-model gating**: детерминированный retrieval top-k похожих заметок (ChromaDB + NetworkX) перед LLM-операциями; порог evo_threshold решает, трогать ли старые заметки. Редкий пример **детерминированного gate перед дорогой LLM-операцией перезаписи**.
- **Роль модели**: возвращает структурированные заметки/ссылки/решения об обновлении; применяет код. Эволюция старых заметок model-driven (LLM переписывает description).
- **Write-path gating**: агент-харнесс; human gate нет.
- **Provenance/audit**: слабее Zep — контекст и время есть, bi-temporal invalidation нет; эволюция переписывает без гарантированной истории версий.
- **Forgetting/decay**: формального нет; «забывание» = выпадение из графа при плохом линковании.
- **Eval**: LoCoMo multi-hop ROUGE-L ~5.8x против vector-базлайна (GPT-4o-mini); дешёвые per-operation затраты.
- **Failure modes**: (1) эволюция старых заметок model-driven и необратима без версионирования («evolution of old notes is model-driven and unverifiable»); (2) каскадная перезапись может деградировать хорошие заметки; (3) research-grade код (Chroma+NetworkX), не production-инфраструктура; (4) порог по умолчанию делает эволюцию либо редкой, либо шумной.
- **Преемники и линия**: **MEMI** (ICLR 2026, Zhou et al.) — noise filtering через distillation: агенты, обученные *отбрасывать* нерелевантное, превосходят в long-horizon задачах на порядок. **SimpleMem** (arXiv:2601.02553, 2026) — «эффективная lifelong-память»: минимизация записей в пользу точных. Общий вектор: от «записывай всё и организуй» к «не записывай лишнее» — обучаемый admission control (Adaptive Memory Admission Control в arXiv:2608.00303; **BudgetMem**, arXiv:2511.04919 — обучаемые политики выборочной памяти под бюджет токенов).
- **Advanced vs marketing**: genuinely advanced как исследование организации памяти (динамическая структура вместо фиксированной схемы — новизна по NeurIPS). Не production-система.
- **Передаваемые механизмы**: дешёвый детерминированный порог перед перезаписью старого; атомарные заметки со structured metadata; bidirectional links; эволюция затронутых записей при новых фактах.

---

## 5. Академический фронтир 2025–2026

### 5.1 Sleep-time Compute (Lin et al., arXiv:2504.13171)

Первоисточник концепции «сна»: модель в idle-время **предвычисляет** по контексту ответы на предполагаемые будущие запросы. Stateful GSM-Symbolic/AIME: −~5x test-time compute при той же точности; до +13%/+18% точности при масштабировании; Multi-Query амортизация −2.5x/запрос. Прямой вывод: польза ∝ предсказуемость запросов. Для memory-дизайна это значит: offline-консолидация окупается, когда паттерн будущего использования памяти угадываем из истории; плохо предсказуемые домены выигрывают меньше. Это единственная работа, давшая **количественную кривую** «сколько стоит сон vs сколько даёт».

### 5.2 SleepGate (arXiv:2603.14517, март 2026)

«Learning to Forget: Sleep-Inspired Memory Consolidation» — борьба с **proactive interference** в KV-cache: устаревшие значения доминируют в attention даже при соседстве с query (PI-LLM, Wang & Sun 2025). Три sleep-модуля: **key decay** (синаптический даунскейлинг), **learned gating**, **consolidation** (сжатие cache + replay). Уникальный ракурс: забывание как активная операция над состоянием модели, а не над хранилищем фактов. Для агентных сторов переносимо идеей: «proactive interference» аналогична тому, как старые записи памяти перебивают новые при retrieval — нужен активный механизм гашения, а не только ранжирование.

### 5.3 LightMem (arXiv:2510.18866, ICLR 2026; zjunlp/LightMem)

Atkinson-Shiffrin модель в три стадии: **sensory memory** (лёгкая компрессия + topic-grouping фильтр), **short-term** (topic-aware консолидация групп), **long-term со sleep-time update** — новые записи сначала хранятся с timestamp для «мягкого» (soft) немедленного обновления, а **офлайн, во «сне»**, система реорганизует, дедуплицирует, абстрагирует и разрешает противоречия, усиливая кросс-связи. LongMemEval/LoCoMo: +7.7%/+29.3% QA accuracy, до −38x/−20.9x токенов и до −30x/−55.5x API-вызовов; чисто онлайн-стоимость ниже на до 106x/117x. **Важный механизм: двухрежимная запись (soft online + hard offline consolidation)** — факт доступен сразу, но «кристаллизуется» позже. Successors: LightMem-Ego (EMNLP 2026, повседневная память с online editing), LightMem-SLM.

### 5.4 Titans (Google, arXiv:2501.00663, NeurIPS 2025)

Не агентное хранилище, а **neural memory**: memory-модуль (MLP) обновляет веса на inference через surprise-driven gradient (memory как meta in-context learner; запоминает то, что «удивляет»). Три варианта интеграции: Memory-as-Context / Memory-as-Gate / Memory-as-Layer; слои persistent + short-term (attention) + long-term (neural). Вопросы: контроль, аудит, стирание отдельных фактов из весов принципиально трудны (unlearning в fast weights). Значение для агентов: метрика **surprise** как сигнал приоритета записи; контраст с политиками «пиши всё». MIRAS (Google Research, декабрь 2025) — дальнейшее развитие той же линии.

### 5.5 HippoRAG 2 (arXiv:2502.14802, ICML 2025)

Нейробиологическая метафора: LLM = neocortex, KG с **Personalized PageRank** = hippocampus, retrieval encoder = parahippocampal cortex. Offline: LLM (OpenIE) строит open KG из passages. Online: query → seed nodes → PPR по графу → мультихоп-эвиденс за один проход (дешевле итеративного multi-hop RAG). HippoRAG 2 добавляет: интеграцию passages в PPR (dense-sparse fusion), три режима query linking (NER-to-node / query-to-node / query-to-triple, последний — лучший), онлайн LLM-фильтрацию нерелевантных triples. Урок: **ранжирование как графовый процесс с детерминированным ядром (PPR), а не как top-k косинусной близости**; retrieval-процесс, а не хранилище — главный носитель «памяти».

### 5.6 Language Models Need Sleep (Behrouz et al., arXiv:2606.03979, ICLR 2026 submission)

Sleep-фаза: N **offline recurrent passes** по накопленному контексу с обновлением fast weights (state-space memory) — upward distillation (консолидация) + **Dreaming для self-improvement** (генерация синтетических упражнений). Конвергенция offline-проходов улучшает continued-pretraining-style задачи (SQuAD и др.). Уникально: сон как **оптимизационный процесс с проверкой сходимости**, а не как однопроходная суммаризация. Для агентных систем: идея «сверять сходимость по бенчмарк-пробеке перед тем, как принимать результат сна» переносима на пакетную консолидацию.

### 5.7 RecMem (ACL 2026 Findings, arXiv:2605.16045)

Recurrence-based consolidation: эпизодическая память периодически **перегенерируется** из своего собственного текста (merge-промпты), семантическая — дистиллируется из эпизодической. Заявлено −87% token cost против eager-consolidation подходов при сохранении точности. Механика похожа на «периодический rebuild вместо инкрементальных правок» — принципиально иная альтернатива receipts-цепочке: не накапливать правки, а периодически пересобирать слой.

### 5.8 Прочие заметные работы 2026

- **Elastic Memory via Knowledge Crystallization** (arXiv:2608.00303): memory budget как ограничение; «кристаллизация» — сжатие эпизодов в устойчивые знания под бюджетом.
- **SodaMem** (arXiv:2608.08055): evidence-grounded temporal graph — связь фактов с цитируемыми evidence-passages (противовес Zep-стилю экстракции без evidence).
- **MemMachine**: 0.917 на LoCoMo через episodic storage с ground-truth-preserving дизайном (сохранение raw вместо перезаписи).
- **RaMem** (arXiv:2606.22844): contextual reinstatement — восстановление контекста эпизода при retrieval.
- **Memory-R1 / NeuSymMS**: RL-обучение lifecycle-решений о памяти и не-LLM классификаторы жизненного цикла (CLIPS-style) — цикл записей решается обученной политикой/правилами, а не промптом.
- Пейзаж naming 2026: NapMem (pyramid), GAM (write-isolated buffers + thematic consolidation), REMem (gist→fact), CAST, MRAgent — общий паттерн: **write-isolation** (сырое пишется без LLM, обработка — отдельным проходом) и **иерархическая гранулярность**.

---

## 6. Коммерческие и продвинутые системы

### 6.1 Anthropic: Claude Code Auto Dream, Claude Dreaming, CL-Bench

Источники: инженерный блог Anthropic «The components of agent memory» (6 ноября 2026), анализ утечки исходников Claude Code 2.1.88 (31 марта 2026, cli.js.map), сводки (extrapolator, eraofminds, strategro, blog.imseankim).

**Механика (из утечки исходников, services/autoDream/):**
- **Триггер — трёхвентильный**: (1) ≥24 часа с последнего цикла; (2) ≥5 сессий с последней консолидации; (3) consolidation lock против конкурентных процессов. Оба временных порога обязаны сработать — одна марафонская сессия или десять быстрых сессий за два часа не запускают.
- **Четыре фазы**: Orient (оценка состояния памяти) → Gather Recent Signal (сбор инсайтов из недавних сессий) → Consolidate (слияние с существующей памятью) → Prune and Index (удаление избыточности, удержание MEMORY.md ≤ 200 строк / ~25KB).
- **Claude Dreaming (Managed Agents, май 2026; POST /v1/dreams, beta headers managed-agents-2026-04-01 / dreaming-2026-04-21)**: фоновый проход сверяет memory-заметки с session transcripts как ground truth — ловит ошибки и переписывает не соответствующие transcript заметки. Важно: исходный memory store сохраняется — dreaming производит **новый store, не изменяя входы** (outputs reviewable, откат = старый store). Harvey (enterprise-клиент): 6x больше завершённых задач после включения.
- **Из инженерного блога (6.11.2026)**: качество кураторства важнее объёма — 9 точечных файлов > 413 файлов (Pokémon Red: 4 бейджа против 1); жёсткие правила «record only failures» **снижают** пользу памяти для сильных моделей; устаревающие детальные записи деградируют поведение (Sonnet 3.7 создавал 6 файлов и удалял 9, не совершая игровых действий); память определяется как веса-NЕ-знание: заметки между сессиями, read/write через директорию.

- **Trigger/cadence**: периодический + счётчик сессий + lock; у Managed Agents — по запросу на 1–100 сессий.
- **Pre-model gating**: детерминированные пороги (24h, 5 сессий, лимит 200 строк/25KB) — типичная схема «жёсткий gate → LLM-проход → жёсткий лимит после».
- **Роль модели**: свободное переписывание заметок, но с ground truth (transcript) и внешним лимитом; кто применяет — харнесс (Auto Dream-сервис), не сам агент.
- **Write-path gating**: фоновый сервис пишет в memory-директорию; человек может отключить (autoDreamEnabled=false) и править файлы руками; cc-janitor добавляет snapshot/undo вокруг каждого цикла.
- **Provenance/audit**: transcript = ground truth для валидации (редкий пример «сон сверяет себя с первоисточником»); версионирование — через файловую систему/бэкапы, receipts first-class нет.
- **Forgetting/decay**: Prune-фаза + 200-строчный бюджет; transcripts чистятся по cleanupPeriodDays.
- **Eval**: **CL-Bench 1.0** (Continual Learning Bench, экспертно-валидированные задачи в реальных stateful-средах) — Claude Opus 5 первым «замкнул learning loop» (правило + evidence + ответы на ранние вопросы + предупреждения о ловушках); Sonnet 4.6 подъём почти 2x от явного промптинга «читай память и проверяй теории», Sonnet 5 делает это автономно (9/10). Pokémon Red как long-horizon игровой прогон.
- **Failure modes (их же слова)**: заметка, ошибочная с начала или дрейфующая, читается и доверяется позже (Mt. Moon — падение с уступа 3 раза подряд до исправления заметки); нет опубликованных cadence/eviction/conflict-политик для Managed Agents; непереносимость dreaming-памяти за пределы платформы не оговорена.
- **Advanced vs marketing**: самый сильный из корпоративных сигналов — публичная механика (через утечку), собственный бенчмарк (CL-Bench), ground-truth валидация. Marketing: 6x Harvey — одна цифра одного клиента, без контрольной группы публично.
- **Передаваемые механизмы**: трёхвентильный триггер (время × объём × lock); фиксированный бюджет индексного файла; «свойство обратимости» (новый store, входы нетронуты); transcript-валидация заметок; строгие правила хуже свободы суждения для сильных моделей.

### 6.2 MemOS (MemTensor, arXiv:2507.03724; кодовый разбор v2.0.10)

**Архитектура.** «ОС памяти»: три слоя (Interface — MemReader/MCP/REST; Operation — MOSCore, MemScheduler, MemLifecycle, MemFeedback; Infrastructure — Neo4j/Qdrant/Milvus/MySQL/Redis). Базовая единица — **MemCube**, контейнер до 4 слотов: plaintext (naive/general/tree), **activation (KV-cache)**, **parametric (LoRA, пока stub)**, preference.

- **Trigger/cadence**: async через MemScheduler (Redis Streams, priority queues, per-user isolation): ADD_TASK на новые сообщения, отдельные задачи на документы/преференции; фоновый MemoryManager периодически реорганизует tree-память (merge, dedup, relation building); рабочая память периодически «промоутится» в long-term.
- **Pre-model gating**: MinHash dedup (preference), BM25+vector dual retrieval, реранкеры с весами уровней иерархии (topic→concept→fact); PRO-режим — LLM декомпозиция запроса на саб-вопросы перед поиском.
- **Роль модели**: MemReader извлекает структурированные TextualMemoryItem; relation-detection (conflict/duplicate/complementary) — LLM; MemFeedback: natural-language feedback → judgment → comparison → операция (update/delete/supplement), en+zh промпты.
- **Write-path gating**: пишет сервис по API/MCP; роли и ACL (UserManager, role-based); человек — через Memory Viewer/feedback API.
- **Provenance/audit**: самая богатая метадата из обзора: `sources: list[SourceMessage]`, version + `history` (архивные версии), status (activated/resolving/archived/deleted), confidence 0–100, visibility (private/public/session).
- **Forgetting/decay**: статусная машина (archived/deleted) + версионирование; формального временного декея нет.
- **Eval**: LoCoMo 75.8, заявки +40.43% (LongMemEval), +43.7% vs OpenAI Memory, −35.24% memory tokens; встроенные сравнительные скрипты против Mem0/Zep/Memobase/SuperMemory/MemU (часть self-reported).
- **Failure modes**: LoRA-слот — placeholder; KV-cache только с локальными HF-моделями; тяжёлая инфраструктура (5 хранилищ); китайский вендор-стек; LoCoMo 75.8 — ниже лидеров.
- **Advanced vs marketing**: genuinely advanced в абстракции (единый контейнер heterogeneous-памяти, KV-cache как first-class, feedback-loop) и в provenance. Стек развит неравномерно (plaintext/tree — да, parametric — нет).
- **Передаваемые механизмы**: статусная машина памяти с confidence и version history; natural-language feedback → judgment → операция; декомпозиция сложного запроса перед retrieval; skill memory с эвалюцией (генератор/эвалюатор/эволвер).

### 6.3 cognee

Источники: cognee.ai (docs, сравнения), GitHub topoteretes/cognee, миграционные туториалы (issues #3399).

**Механика.** ECL-пайплайн (Extract → Cognify → Load) над 38+ форматами: каждая инжестия строит **и граф сущностей/отношений, и векторный индекс**, оба участвуют в поиске. Самопозиционирование: «self-evolving memory that curates itself» — память как knowledge layer, который «рассуждает» (graph+vector hybrid с pluggable backends: Neo4j, Neptune, Postgres/pgvector, Kuzu, Redis, LanceDB, Qdrant). Импортёры из Letta (.af-файлы → блоки+эпизоды) и Zep (dumps → эпизоды+entities+facts) — признак зрелости экосистемы (память стала переносимым активом).

- **Trigger/cadence**: ingest-time; отдельного dreaming/consolidation-цикла публично не описано — «curation» в основном декомпозиция при загрузке.
- **Pre-model gating**: не раскрыто детально (open-core, но внутренности пайплайна Cognify — LLM-heavy).
- **Роль модели**: экстракция графа; применяет движок.
- **Write-path gating**: API; human-in-the-loop нет first-class.
- **Provenance/audit**: граф ребер с атрибутами, но bi-temporal модели Zep нет.
- **Forgetting/decay**: заявлен «single Postgres for graph+vectors+sessions» без отдельного decay-слоя.
- **Eval**: маркетинговые сравнения против Supermemory (proprietary internals недоступны); независимых публикаций с воспроизводимыми цифрами мало.
- **Advanced vs marketing**: умеренно — зрелый ECL и переносимость, но «self-evolving» — в значительной степени упаковка; подходит как knowledge-graph backend, а не как система агентного сновидения.

### 6.4 supermemory

Источники: supermemory.ai/docs (how-it-works, concepts).

**Механика.** Managed memory API: документ любого вида (транскрипты, PDF, URL, коннекторы Drive/Notion/Gmail) → engine сам извлекает и организует (multi-modal ingestion, без предварительного чанкинга со стороны клиента). Граф памяти с тремя типами связей: **Updates** (новое противоречит/меняет — `isLatest` флаг отслеживает актуальное), **Extends** (обогащение без замены), и связи-контексты. Вектор+граф гибридный retrieval.

- **Trigger/cadence**: ingest-time.
- **Pre-model gating**: не раскрыто (proprietary engine, internals not disclosed — по признанию самой cognee в сравнении).
- **Роль модели**: extraction engine; применяет сервис.
- **Write-path gating**: API; пользователь удаляет явно.
- **Provenance/audit**: связи Update/Extends — лёгкая форма supersession, но без bi-temporal меток и source-episodes уровня Zep.
- **Eval**: публично воспроизводимых бенчмарков мало; позиционируется как «memory API для AI-приложений».
- **Advanced vs marketing**: в значительной степени marketing — инженерия скрыта, механика извлечения не опубликована; полезен как инфраструктурный API, но не как источник архитектурных идей. Полезный заимствованный паттерн — типизация связей (Updates/Extends/isLatest) вместо плоского списка фактов.

### 6.5 LangMem (LangChain)

Источники: docs LangMem, обзоры (past.dev, rywalker, datapace, agentsystems.blog).

**Механика.** Три типа памяти по CoALA: semantic (факты), episodic (примеры), **procedural (переписывание собственного system prompt из опыта — уникум среди обзора: ни Mem0, ни Zep этого не делают)**. Два пути записи: **hot-path** (agent tools `create_manage_memory_tool` / `create_search_memory_tool` — агент сам пишет в диалоге) и **background** (`create_memory_store_manager` — экстракция/консолидация после завершения диалога; `ReflectionExecutor` откладывает и дедуплицирует фоновую работу). Storage-agnostic (BaseStore: in-memory → Postgres), нативная интеграция с LangGraph checkpoints.

- **Trigger/cadence**: hot-path — per-turn; background — после завершения сессий (отложенная, с дедупликацией через ReflectionExecutor).
- **Pre-model gating**: дешёвый сигнал до LLM-экстрактора фактически отсутствует (extraction prompt на всю сессию).
- **Роль модели**: hot-path — операции через tools; background — экстракция решений; prompt-optimizer переписывает инструкции.
- **Write-path gating**: агент (hot-path) или менеджер (background); human gate нет; версия PyPI 0.0.30 (октябрь 2025) — pre-1.0.
- **Provenance/audit**: namespace-организация, без temporal validity.
- **Forgetting/decay**: нет формального; unbounded memory предотвращается только консолидацией.
- **Eval**: официальных бенчмарков LangMem-стека мало; цитирование чужих (Mem0 LoCoMo) для latency-сравнений.
- **Failure modes**: pre-1.0; episodic-память «pattern you implement yourself» (не shipped opinionated); procedural-память опасна без review (переписанный системный промпт не проходит человека).
- **Advanced vs marketing**: procedural memory + двухпутевая схема (hot/background) — реально полезная архитектурная идея; остальное — скромные примитивы.
- **Передаваемые механизмы**: разделение hot-path write и background consolidation; procedural memory как отдельный слой; отложенный исполнитель с дедупликацией фоновых задач.

### 6.6 MemoryOS / Second Me, Cognition/Devin, прочие

- **Second Me** (from Krafton/мейнстрим Awesome-Agent-Memory): tree-structured hierarchical memory + «dream»-фаза сжатия эпизодов в дерево знаний; публично опубликованная механика тонкая (препринт + продукт), деталей уровня первоисточника не найдено — **низкая уверенность**. Идея «личной памяти как дерева, сжимаемого во сне» близка к tree-подходу MemOS.
- **Cognition/Devin**: публично документированных механик долговременной памяти/Devin-style consolidation не найдено — фирменный туман; не включать в сравнение как источник механик.
- **Cloudflare / MemoryLake / Memobase / Mnemoverse и пр.** — кластер managed memory API второго эшелона; единственное стоящее: **Memobase** — user-profile-центричная структурированная память (полевые профили вместо фактов-строк), и **GoogleCloud always-on-memory-agent** (no vector DB, LLM-driven организация) как индикатор вендорского тренда.

---

## 7. Классические базовые линии (для контраста)

### 7.1 Generative Agents (Park et al., UIST 2023)

**Memory stream** (append-all наблюдений) + retrieval score = α·recency + β·importance + γ·relevance; **reflection** — периодический LLM-синтез высокоуровневых выводов из накопленного stream (threshold по сумме importance). Прородитель идеи «важность × давность × релевантность» и иерархической суммаризации. Ограничения, очевидные задним числом: reflection генерирует красивые, но неверифицированные абстракции; нет ни источников, ни supersession; memory stream растёт неограниченно. Всё же — первый систематический retrieval-score с тремя детерминированными компонентами.

### 7.2 MemoryBank (Zhong et al., AAAI 2024, arXiv:2305.10250)

Первый принципиальный **decay**: forgetting по кривой Эббингауза R = e^(−t/S), где S растёт при retrieval (память, которую вспоминали, забывается медленнее — reinforcement при чтении). Плюс nightly summarization (daily event summaries, user portrait). Для дизайна: retrieval — это не только чтение, но и **обучающий сигнал для retention**; заменяет ad-hoc «популярность».

### 7.3 CoALA (Sumers et al., TMLR 2024)

Каноническая таксономия: working memory (текущий контекст) + episodic + semantic + procedural long-term; решения агента = reasoning over memory + action selection. Само по себе не система, но: (1) vocabulary, которым пользуются все выше; (2) прямое указание авторов, что **memory modification/deletion — «understudied»** — то, чем занимаются dreaming-системы 2026-го.

### 7.4 Reflexion (Shinn et al., 2023) и RL-линия

Verbal reinforcement learning: неудача → self-reflection текстом → сохранение в episodic buffer → conditioning следующей попытки. Для dreaming-дизайна: консолидировать стоит не только факты, но и **исправленные ошибки/стратегии** (policy-level память), и сна должно быть видно, от какого негативного эпизода родилось правило (receipt с failure-ссылкой). Современные наследники: ExpeL (insights из траекторий), Agent Workflow Memory (индуктивные workflow-правила), MEMI (выбрасывание шума).

---

## 8. Сводная таблица сравнения

Легенда: ✓ = есть, ~ = частично/слабо, ✗ = нет, «—» = не раскрыто.

| Система | Тип консолидации | Trigger/cadence | Pre-model gating | Модель возвращает | Кто пишет durable | Human gate | Provenance/audit | Decay/forgetting | Ключевой бенчмарк | Главный документированный failure mode |
|---|---|---|---|---|---|---|---|---|---|---|
| Letta/MemGPT (+ sleep-time) | self-edit + offline sleep-агент | per-turn (event) + idle/compaction | ~ (лимиты блоков) | tool-операции | сам агент / sleep-агент | ✗ (ADE/API) | ~ (git в Letta Code) | ✗ | DMR 32→92.5%; sleep −5x compute | memory divergence sleep-агента от основного |
| Zep/Graphiti | на записи, streaming | per-episode, delta | ✓ entity-resolution dedup | структ. операции add/merge/invalidate | ingestion-пайплайн | ✗ | ✓✓ (bi-temporal, episodes) | ✗ (append-only) | DMR 94.8; LME 63.8 (indep.) | ошибка LLM-invalidation необратима |
| Mem0/Mem0g | extract→update на сообщение | per-message, sync | ✓ retrieval top-k перед Update | JSON ADD/UPDATE/DELETE | сервис по API | ✗ (delete API) | ✗ | ✗ (DELETE-операция; decay 2026 заявлен) | LME 92.5 self / 49 indep. | dense-only retrieval; перезапись без истории |
| A-MEM | write-time организация + эволюция | per-event + каскад | ✓ порог похожести перед эволюцией | заметки/ссылки/решения | harness | ✗ | ~ | ✗ | LoCoMo multi-hop ×5.8 ROUGE-L | необратимая model-driven перезапись заметок |
| LightMem | 3-стадии, soft online + hard offline | per-event + sleep-time | ✓ sensory-filter, topic grouping | суммари/группы/операции | конвейер | ✗ | ~ (timestamps) | ✗ | LME +7.7%, LoCoMo +29.3%, −38x токенов | слабые места не раскрыты авторами детально |
| SleepGate | sleep над KV-cache | периодический sleep | ✓ conflict-aware temporal gate | gate-решения | архитектура модели | ✗ | ✗ | ✓ активное (key decay) | PI-LLM задачи | применимость только к своим весам |
| LMs Need Sleep | offline recurrent passes + dreaming | sleep-фаза | ✓ проверка сходимости | обновление fast weights | сама модель | ✗ | ✗ | ✓ (consolidation) | SQuAD continued | только модельный уровень, не стор |
| RecMem | recurrence-based rebuild | периодический rebuild | — | перегенерация слоёв | конвейер | ✗ | ~ | ✓ (пересборка) | −87% tokens vs eager | потеря деталей при rebuild |
| Claude Auto Dream | 4-фазный фоновый цикл | 24h × ≥5 сессий × lock | ✓ жёсткие пороги и лимиты (200 стр/25KB) | свободное переписывание по transcript | сервис autoDream | ~ (toggle, ручное правка) | ✓ transcript как ground truth | ✓ prune-фаза | CL-Bench; Pokémon Red (9 файлов > 413) | дрейф заметок читается и доверяется позже |
| Claude Dreaming (API) | валидация заметок vs sessions | по запросу (1–100 сессий) | ✓ | новый memory store | платформа | ✓ review store до замены | ✓ (входы нетронуты, store версионируется) | ✓ | Harvey 6x (кейс) | конфликт-политики не опубликованы |
| MemOS | scheduler + background MemoryManager | async очереди, приоритеты | ✓ MinHash/BM25/реранк | структ. items + relation-операции | сервис, ACL | ~ (viewer/feedback) | ✓✓ (sources, version, confidence, status) | ~ (статусы) | LoCoMo 75.8; заявки +40% | LoRA-stub; тяжёлый стек; ниже лидеров |
| LangMem | hot-path + background manager | per-turn + после сессий | ✗ | tool-операции / экстракция | агент / менеджер | ✗ | ~ | ✗ | (нет своих) | procedural-память без review |
| cognee | ECL на ingest | ingest-time | — | граф+вектор | движок | ✗ | ~ | ✗ | маркетинг | «self-evolving» в значительной части упаковка |
| supermemory | engine-managed | ingest-time | — | обновления/extends | сервис | ~ (delete) | ~ (isLatest-связи) | ✗ | мало публичных | internals не раскрыты |
| Generative Agents | reflection | по порогу суммарной важности | ✓ score до LLM (recency×importance×relevance) | рефлексии/наблюдения | runtime | ✗ | ✗ | ~ (утопление в stream) | симуляция фейковых городов | неверифицированные абстракции |
| MemoryBank | nightly summary + decay | nightly; Ebbinghaus по времени | ✓ формула до любых выводов | суммари/портрет | runtime | ✗ | ~ (timestamps) | ✓ R=e^(−t/S), S↑ при retrieval | 10-day user study | портрет-декей жёсткий, неадаптивный |

### Что видно из таблицы (поперечные наблюдения)

1. **Никто, кроме Anthropic (Dreaming API) и частично MemOS, не строит write-path вокруг человека.** Человеческая ратификация — практически пустая ниша; почти все системы доверяют LLM-операциям безвозвратно.
2. **Два полюса provenance**: append-only с bi-temporal метками (Zep) против перезаписи без истории (Mem0, A-MEM, Letta-блоки). Обратимость коррелирует с доверием к системе.
3. **Детерминированные gate'ы перед LLM — редкость и маркер зрелости**: Zep (entity resolution), A-MEM (порог эволюции), Anthropic (вентили + лимиты), SleepGate (conflict-aware gate). Большинство систем платят LLM за «всё подряд».
4. **Decay как first-class механизм почти исчез** после MemoryBank: современные системы либо append-only, либо prune в offline-фазе. Обучаемый admission control (MEMI, BudgetMem) — возвращающийся тренд.
5. **Sleep/dream-системы делятся на «валидирующие» (Anthropic: сверка с transcript) и «генерирующие» (Letta, LightMem: переработка контента)**. Валидирующие безопаснее; генерирующие мощнее и опаснее (divergence).

---

## 9. Уникальные различители (механики, встречающиеся ровно в одной системе)

1. **Bi-temporal факты с non-destructive invalidation** — Zep/Graphiti (valid×transaction time; invalidate ≠ delete). Ни у кого больше нет полноценной двухвременной модели.
2. **Трёхвентильный триггер сна (время × число сессий × lock)** — Claude Code Auto Dream. Двухпороговая AND-логика + конкурентность-лок — эталон детерминированного gate.
3. **Обратимость сна: входы неизменны, выход — новый store** — Claude Dreaming API (Managed Agents). Сон как чистая функция store→store'.
4. **Dreaming-валидация заметок против session transcript как ground truth** — Anthropic. «Сон, который фактчекает себя по первоисточнику».
5. **Переписывание собственного system prompt как procedural memory** — LangMem. Агент, редактирующий собственные инструкции по опыту.
6. **KV-cache как first-class memory-слот** — MemOS (extract/store/inject past_key_values; только локальные модели).
7. **MemCube-контейнер с confidence, version history и status-машиной** — MemOS (activated/resolving/archived/deleted; history архивных версий).
8. **Surprise-driven запись в веса на inference** — Titans (запоминается то, что удивляет; memory MLP обучается на тесте).
9. **Conflict-aware temporal gate против proactive interference** — SleepGate (активное гашение устаревших ключей в KV-cache; «забывание» как операция).
10. **Обучаемый admission control / дистилляция шума** — MEMI + BudgetMem + Adaptive Memory Admission (решение «записывать ли» обучается, а не промптуется).
11. **Natural-language feedback → judgment → операция над памятью** — MemOS MemFeedback (человек говорит «это не так» — пайплайн сам превращает в update/delete/supplement).
12. **Recurrence-based rebuild вместо инкрементальных правок** — RecMem (−87% токенов; периодическая перегенерация слоя, а не receipts-цепочка).
13. **Zettelkasten-эволюция: новая заметка пересматривает старые по детерминированному порогу** — A-MEM (evo_threshold как дешёвый gate перед дорогой перезаписью).
14. **Soft online + hard offline запись** — LightMem (факт сразу доступен с timestamp, «кристаллизуется» во сне).
15. **Игровой long-horizon прогон как eval памяти** — Anthropic (Pokémon Red: кураторство > объём; метрика «действия vs создание/удаление файлов»).

---

## 10. Выводы для дизайна «proposals queue + human ratification + receipts chain»

Что подтверждается обзором, что противоречит и что расширяет дизайн на базе очереди предложений, человеческой ратификации и цепочки receipts:

### Подтверждается

1. **Human gate — редкость, и это фича, не баг дизайна.** Ни одна известная система не строит write-path вокруг человека; «ратификация предложений» — незанятая ниша с прямым обоснованием в литературе: model-driven перезапись необратима и ошибки накапливаются (Zep-invalidation, A-MEM-эволюция, Mem0-UPDATE). Рецепты + одобрение человека закрывают главный системный failure mode всей линии.
2. **Receipts (source-ссылки, validity windows, supersession) — зрелый, проверенный паттерн.** Zep доказал ценность episode-as-provenance и bi-temporal меток; MemOS — ценность version history + confidence + status. Цепочка receipts ложится прямо в эти доказанные гнёзда.
3. **Детерминированные gate'и перед LLM окупаются.** Каждая система с ними (Zep, A-MEM, Anthropic, SleepGate) именно ими и выделяется; очередь предложений сама по себе — такой gate (дёшёвый детерминированный вход перед дорогой записью).
4. **Непереносимые записи без истории — антипаттерн.** UPDATE-перезапись (Mem0) и свободная правка блоков (Letta) — источники документированных деградаций; supersession-цепочка с сохранением предков правильна по консенсусу данных.

### Противоречит / что пересмотреть

1. **«Жёсткие правила записи хуже свободы суждения» (Anthropic, прямые эксперименты).** Дизайн, где ратификатор проверяет compliance с жёсткой схемой («записывай только провалы»), может съесть пользу памяти у сильных моделей. Вывод: человек должен ратифицировать *факты и их evidence*, а не навязывать категории/формат записи; пороги отбора предложений лучше держать мягкими.
2. **Объём ратификации не масштабируется линейно, и это главный риск дизайна.** Если сон генерирует много предложений, человек станет bottleneck'ом. Контрпример из практики: качество кураторства важнее количества (9 файлов > 413). Значит очередь должна иметь **агрессивный детерминированный пред-фильтр** (dedup, пороги похожести A-MEM-стиля, confidence MemOS-стиля) до того, как человек увидит хоть одно предложение. Иначе ратификация превратится в либо rubber stamp, либо очередь-мертвяк.
3. **Сон ≠ только предложения на запись.** У всех продвинутых систем сон делает ещё три класса работ: (а) валидация существующего (Anthropic-сверка с transcript); (б) prune/decay (Auto Dream фаза 4); (в) reorganизация связей (LightMem, MemOS tree). Если очередь принимает только «новые факты», система будет копить устаревшее и разваливаться по связям — это именно тот failure mode Sonnet 3.7 в Pokémon Red (413 файлов, дрейф). Предложения должны включать операции классов UPDATE/SUPERSEDE/MERGE/DELETE/VALIDATE с равным статусом в очереди.
4. **Единичная ратификация не защищает от дрейфа после записи.** Запись верна в момент одобрения, но устаревает (правило Эббингауза + смена мира). Нужен либо periodic re-validation pass (Anthropic-модель), либо validity windows (Zep-модель), либо retrieval-driven reinforcement (MemoryBank: чтение повышает retention) — одной receipts-цепочки недостаточно, она фиксирует происхождение, но не актуальность.
5. **Sleep-агент, пишущий напрямую, дивергирует (Letta).** Архитектура «сон → предложения → человек» сама по себе лечит divergence — предложения не применяются автоматически. Но дивергенция имеет и вторую ось: сон может научиться генерировать предложения, которые человек одобряет, но которые ухудшают поведение основного агента. Нужен **antidivergence-пробник**: после применения пачки предложений — автоматическая проверка (regression-прогон/бенчмарк-проба LMs-Need-Sleep-стиля) до массового применения следующей пачки.
6. **Eval-парадокс: все лгут про свои цифры.** Расхождение self-reported vs independent у Mem0 (92.5 vs 49) — предупреждение: своя система receipts + ratification должна измеряться независимым протоколом (LoCoMo/LongMemEval/CL-Bench/MemoryBench со shared-стеком), иначе дизайн будет оптимизирован под собственные метрики.

### Расширяет (что добавить к дизайну сверх очереди)

1. **Вентильный триггер сна** — не cron, а AND-комбинация (время × объём накопленного × lock), как Auto Dream; у родительского контура уже есть «daily, not per tick» — совпадает с лучшей практикой.
2. **Очередь должна быть транзакционной и двуфазной**: пакет предложений → dry-run preview (diff-представление для человека, Anthropic-store-стиль: входы нетронуты до применения) → batch approve → применение с atomicity. Dreaming API показывает ценность «store→store'» семантики.
3. **Receipts — не только прошлое, но и будущее**: к каждому предложению приклеить validity window и условие инвалидации (Zep), чтобы сон мог потом предложить SUPERSEDE, опираясь на ту же цепочку.
4. **Классы памяти в очереди**: facts, preferences, procedures (LangMem), skills (MemOS) — ратификация procedural/skills должна быть строже (они меняют поведение агента, а не контент).
5. **Метрики очереди как сигнал здоровья**: длина очереди, время до ратификации, доля отклонённых, «токены на retrieval» (Mem0), drift-статистика (zeroclaw memory stats уже есть — расширить drift-видимостью на уровне операций).
6. **Забывание как операция очереди**: DELETE-предложения с decay-обоснованием (Ebbinghaus/BudgetMem) должны проходить тот же human gate — удаление опаснее записи, а в обзоре удаление почти везде либо LLM-решение (Mem0), либо отсутствует (Zep append-only).
7. **Обучаемый пред-фильтр как эволюция очереди**: когда накопятся ратификационные решения человека, пороги пред-фильтра можно дообучать на этих метках (MEMI/BudgetMem-подход) — human decisions становятся обучающими данными, а не вечной нагрузкой.

---

## 11. Источники

Первоисточники:
- Sleep-time Compute: https://arxiv.org/abs/2504.13171 ; код https://github.com/letta-ai/sleep-time-compute
- Zep: https://arxiv.org/abs/2501.13956 ; https://github.com/getzep/graphiti
- Mem0: https://arxiv.org/abs/2504.19413 ; https://docs.mem0.ai/core-concepts/memory-evaluation
- A-MEM: https://arxiv.org/abs/2502.12110 (NeurIPS 2025)
- LightMem: https://arxiv.org/abs/2510.18866 ; https://github.com/zjunlp/LightMem
- Titans: https://arxiv.org/abs/2501.00663 ; https://research.google/blog/titans-miras-helping-ai-have-long-term-memory/
- HippoRAG 2: https://arxiv.org/abs/2502.14802
- SleepGate: https://arxiv.org/html/2603.14517
- Language Models Need Sleep: https://arxiv.org/abs/2606.03979 ; https://openreview.net/forum?id=iiZy6xyVVE
- MemOS: https://arxiv.org/abs/2507.03724 ; кодовый разбор https://lin-guanguo.github.io/llm-memory-research/memos.research/ ; https://tomrochette.com/agents/memory/memos/
- Anthropic: инж.блог 6.11.2026 «The components of agent memory» (сводки: https://extrapolator.ai/2026/11/07/anthropic-details-claude-dreaming-for-agent-memory-consolidation/, https://max.nardit.com/articles/anthropic-agent-memory-components-dreaming-memory-md); утечка исходников Claude Code 2.1.88: https://claudefa.st/blog/guide/mechanics/claude-code-source-leak (autoDream: триггер + 4 фазы); Dreaming API: https://eraofminds.com/blog/how-to-use-claude-dreaming/
- Letta: https://www.letta.com/blog/why-memory-isnt-a-plugin ; обзор https://seekvana.com/library/agentic-ai/memory-research-memgpt-letta-sleep-time
- MemoryBank: https://arxiv.org/abs/2305.10250 ; Generative Agents: Park et al. 2023 (arXiv:2304.03442); CoALA: Sumers et al., TMLR 2024 (arXiv:2309.02427)
- RecMem: arXiv:2605.16045; MEMI: ICLR 2026 (Zhou et al.); SimpleMem: arXiv:2601.02553; BudgetMem: arXiv:2511.04919; Elastic Memory: arXiv:2608.00303; SodaMem: arXiv:2608.08055; MemoryBench: arXiv:2510.17281; CL-Bench: https://snorkel.ai/research-paper/continual-learning-bench-evaluating-frontier-ai-systems-in-real-world-stateful-environments/

Вторичные обзоры (контекст/кросс-проверки): digitalapplied.com (Mem0 49.0 vs self-reported), callsphere.ai (LME 63.8 vs 49.0), agentsystems.blog (Letta 25k tokens/turn; LangMem pre-1.0), thecontinuitylayer.com (A-MEM «evolution unverifiable»), adityakarnam.com, local-ai-zone.github.io, kmail.at / beri.net (Graphiti bi-temporal), memorypapers.org (benchmark-карточки).

Оговорки по уверенности: Second Me — механика тонко опубликована (низкая уверенность); Letta «Memory Models 2026» — по вторичным пересказам; Cognition/Devin — публичных механик не найдено; цифры Harvey 6x и sleep-time 5x — вендорские, без независимой репликации; часть arXiv-идентификаторов 2026 года принадлежит препринтам под рецензией.
