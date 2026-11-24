# PR-MAP — реестр апстрим-кампании (RAM-RAMII)

Единая точка истины по открытым PR в zeroclaw-labs/zeroclaw. Обновляется при
каждом движении по любому PR. Правило контура: новая фича/фикс — сначала
`local/fork-main` + живой тест, потом PR отдельной веткой от `origin/master`.

## Телеграм-кластер (reply-медиа и voice)

| PR | Суть | База / зависимости | Ревью | Следующий шаг |
|---|---|---|---|---|
| **#12404** | reply-документы агенту как файлы + caption | `origin/master`, самодостаточен | P1-фикс 15.11: `00f1656cf4` — ReplyPrepOutcome (Ready/SkipPermanent/RetryTransient), оба caller'а на retry-пути, dispatch-регрессия (getFile 500→200); reply 88/0, document 34/0, media_group 18/0; ответ ревьюеру отправлен. **greptile P1 17.11 на том же коммите**: retry-ветка держит offset ДО acceptance-проверки (mention_only × passive_group_context) — «ignored replies block later messages». **ТРИАЖ: валиден; ФИКС ЗАПУШЕН 19.11** (`761cdaeaa`, ff): REPLY_PREP_MAX_ATTEMPTS=3 на общем transient_retry-счётчике, оба caller'а (update + media-group batch), регрессионный тест, telegram 437/437; ответ greptile в PR. **РЕБЕЙЗ 21.11 (head `e6086dde5e`)**: на мастер, 0 конфликтов (мастер telegram.rs не трогал) — Security-фиксы подтянуты, красный Parallel Runtime Test (`broadcast_hook…` в runtime, не дифф) перезапущен пушем (re-run джобы недоступен — нет админ-прав); верификация: channels lib 2043/5 (все 5 — `link_enricher::redirect_policy_*`, нужен прямой интернет, окружение); комментарий + пинг tidux/Audacity88. **CI Required Gate ЗЕЛЁНЫЙ на head (21.11 вечер)** | ждём re-review Audacity88 (tidux CHANGES_REQUESTED 16.11 висит — наш P1-фикс 15.11 его закрывает, ревьюер ещё не пересматривал) |
| **#12405** | фото/видео в reply-context | Depends #12404 (draft) | — | снять draft после принятия #12404 |
| **#11601** | transcription retry + voice-drop announce; voice-инфраструктура (notify_voice_drop, VoiceDropReason, transient budget) | `origin/master` | APPROVED финальный (Audacity88 14.11); блокер IftekharUddin 16.10 ИСПРАВЛЕН на head (1f4fe06c9d), но он не пересматривал. **ПИНГ ОТПРАВЛЕН 18.11** — вежливый, с указанием хэда и что правка покрыта апрувом Audacity88 | ждём re-review IftekharUddin → мердж |
| **#12197** | voice paths в reply context (captions ВЫНЕСЕНЫ в #12404) | пересобран: master + #11601 | **ФИКСЫ 18.11 ЗАПУШЕНЫ** (b3908565, ребейз + 🔴 закрыт): topic-root id-check до любого скачивания; consumer-check в каждом чате (sticker/video/location в DM больше не тянут аудио); 72/72 voice/reply_prep + 3 новых регрессии. Остаются 🟡: untranscribed fallback как документ, body; в комментарии — пометка, что ветка несёт #11601 до его мерджа | ждём re-review tidux; 🟡-список следующим |

Владение caption: **только #12404**. В #12197 caption-код не дублировать.

## Смежные voice-PR (не в стеке, но учитывать при ревью)

| PR | Суть | Примечание |
|---|---|---|
| #12201 | voice transcription без mention в mention-only группах | **ПЕРЕСОБРАН 21.11 под матрицу 9-12 (вариант M, виза Ram)**: ветка заново от свежего master, head `b68915be81`, +338/−47 в одном файле; гейт → `MediaAdmission`+`participation_decision`, пассивная голосовая ветка (транскрипция + `passive_context`), флаг `voice_without_mention` отозван (6 файлов конфига больше не тронуты), photo/document → SkipPermanent с пометкой follow-up; captionless-решение не зависит от username-кэша; верификация: 2016/0 (34 link_enricher отфильтрованы — сеть), fmt+hygiene чисто; body переписан, комментарий «why the flag is gone» + пинг tidux. **CI Required Gate ЗЕЛЁНЫЙ на новом хэде (21.11 вечер)**. Анализ: `docs/contour/PR-12201-REBUILD-ANALYSIS-2026-11-21.md`. **tidux COMMENTED 16.11** (на старом дизайне): ребейз-требование закрыто новым хэдом | ждём re-review tidux; follow-up визы: B2-дегрейды, пассивные photo/document |
| #12198 | default_provider fallback для transcription | **tidux CHANGES_REQUESTED 16.11 — БЛОКЕР ЗАКРЫТ 22.11** (`6813069e83`, виза Ram, волна 2): типизированная `ProviderNotConfigured` → SkipPermanent + нотис `VoiceDropReason::ProviderUnavailable` (5 локалей); empty-alias арм тоже обзавёлся нотисом (`NoProviderConfigured`); ребейз на свежий мастер (101 behind, конфликтов 0); верификация: channels lib 2020/0 (1 флаки typing-теста под нагрузкой, зелёный на повторе), fmt+hygiene; I/O-ошибки остались RetryTransient — ровно по скоупу tidux; 🟡 validate default_provider и voice_wake None — осознанно в follow-up (не блокер по tidux); ответ tidux в PR | ждём re-review tidux |
| #11651 | 429 retry_after flood gate | независима |

## Независимые PR (телеграм и рантайм)

| PR | Суть | Статус |
|---|---|---|
| #12371 | edited_message с latest-wins coalescing | **РЕШЕНО 22.11: ЧИНИМ И УДЕРЖИВАЕМ** (виза Ram, волна 3) — #12426 теперь ссылается на эту edit-политику как на consumer, закрытие сделало бы картину несогласованной. **НАХОДКИ tidux ЗАКРЫТЫ** (`cbda758838`, ребейз на мастер): E0063 — 8 литералов (whatsapp_web lib, linq-helper, daemon ×4, registry, src/config); stale-правки до freshness-гейта — SOP-гейт исключает edited + адаптер не шлёт ack/typing для stale (константа EDIT_FRESHNESS_SECS общая); отмена pending-правок при выходе цикла с логом; маркер `[edited earlier message]` для stale-пассива; дока про debounce_ms=0; body: Related #11713 + #11781 как альтернатива. Warning 2 (interrupt + двойной debounce) — честно отложен в follow-up с правкой body. **МИКРОВОЛНА 23.11** (`cd028c96f6`, виза Ram «fix them»): мастер влил #10988/#11104 с новыми init-сайтами ChannelMessage — E0063 ушёл вместе с ребейзом (check all-targets чист), 3 em-dash (schema.rs, channel.rs doc-comments, orchestrator comment) заменены — падал generated-prose gate и Docs Style; channels lib 2028/0; пуш + комментарий в PR | ждём re-review tidux |
| #12346 | owner-notice о неавторизованных | **ФИКСЫ 18.11 ЗАПУШЕНЫ** (80185f5, ребейз + оба 🔴 закрыты): превью больше не несёт URL/markup (strip_untrusted_markup — disguised link убит); per-(alias, owner_chat) throttle 30с на detached-доставку. 🟡 bystander: превью сохранено, но стерильное; предложен count-only вариант. 31/31 тестов. **РЕБЕЙЗ 21.11** (`df887e45c5`), пинг tidux в PR | ждём re-review tidux; 🔵 spawn-path тест + лишние owner_chat_id literals — открыты |
| #12070 | skills-splice уважает prompt cap | Audacity88 APPROVED повторным ревью 16.11 (59456bdea0, «approve the fix»). РЕБЕЙЗ ЗАПУШЕН 18.11: чистый (97 behind), 13/13 splice/prompt_cap, комментарий в PR. Хвост tidux 13.11 закрыт (body 14.11 + xattr-фикс в базе). **greptile 18.11: «no findings outside the diff»** — чисто. **РЕБЕЙЗ 21.11** (`5ff374109b`), пинг tidux в PR | ждём tidux re-review **РЕБЕЙЗ 24.11** (`e4867844c2`, мастер сдвинулся — конфликты orchestrator/mod.rs разрешены: резервирование капа сохранено, новый вызов build_channel_text_tool_instructions мастера принят; check channels all-targets чист; пуш + комментарий) | ждём re-review Audacity88 |
| #12056 | identity-секции в head direct-path промптов | APPROVED (IftekharUddin 09.11) |
| #11910 | fail-closed budget системного промптa | tidux ПОЛНОЕ ревью 16.11 (COMMENTED, не апрувит). **РАБОЧИЙ СПИСОК ЗАКРЫТ 18.11**: ребейз (конфликт scheduler.rs разрешён, 29/29), privacy-правка firstcontact→#11908 (merge gate), 🔵 remediation до cron-terminal, body по шаблону, Fixes→Related, placement row отдельным PR **#12550**. **⚠️ CI КРАСНЫЙ на head bc115bfb (18.11)**: E0063 — фикстура scheduler.rs:2718 без поля `window_is_compatibility_fallback` (пришло с #11578 на мастере); локальный «29/29» был фильтрованный, lib-test целиком не собирался. greptile P1 то же самое + P2 (placement exception — это #12550, в полёте). **CI-ФИКС ЗАПУШЕН 19.11**: `0f072bf05` (fixture + поле, lib-test таргет компилируется, scheduler 75/75), комментарий в PR. **ПАСС 21.11 (head `10035b70c5`)**: comment-hygiene фикс (убраны `#NNNN`-рефы из двух комментариев — гейт ронял Lint), ребейз на мастер (14 behind, 0 конфликтов) — подтянуты wasmtime/advisory-починки (#12610 и др., красный Security был унаследован); верификация: runtime lib 6068/0, config lib 1948/1 (фейл powershell-CJK — окружение), гейт по репо green; комментарий + пинг tidux в PR. **CI Required Gate ЗЕЛЁНЫЙ на head (21.11 вечер)** | ждём мерджа #12550 → re-review tidux/Audacity88 |
| #11934 | agent_alias атрибуция cron/gateway | APPROVED (IftekharUddin 09.11) |
| #11935 | `cron runs` CLI | tidux COMMENTED 16.11 (блокеры resolved). **РЕБЕЙЗ ЗАПУШЕН 18.11**: строка исключения вынесена в record-PR **#12549** (апрув Audacity88 06.11), Wasmtime-бамп снимает красный Security, 217/217 cron-тестов | **ребейз 21.11** (`8ce8780ea7`), гейт должен позеленеть; ждём мерджа #12549 → re-review tidux |
| #11983 | policy refusal с reason/remediation | APPROVED (tidux 13.11) |
| #12204 | web_search compiled-out warning | APPROVED (tidux 13.11) |

## Record-PR'ы ADR-016 (18.11)

| PR | Для | Суть |
|---|---|---|
| #12549 | #11935 | Строка исключения cron run-history read (апрув Audacity88 06.11) — **greptile clean 18.11** (no actionable issues, low impact). **РЕБЕЙЗ 23.11** (`e703cc11c5`, микроволна фиксов, виза Ram «fix them»): конфликт в `zeroclaw-runtime/AGENTS.md` с #11874-строкой разрешён (обе строки), hygiene-гейт green, пуш + комментарий в PR — ждём мерджа до re-review #11935 |
| #12550 | #11910 | Строка исключения fail-closed floor gate (апрув Audacity88 06.11, scope дословно) — **greptile clean 18.11** (no actionable issues; P2 greptile на #11910 = этот PR). **РЕБЕЙЗ 23.11** (`4dd7232875`, микроволна фиксов, виза Ram «fix them»): конфликт в `zeroclaw-runtime/AGENTS.md` с #11874-строкой разрешён (обе строки), hygiene-гейт green, пуш + комментарий в PR — ждём мерджа до апрува #11910 |

## Matrix-кластер (NB-дорожка)

| PR | Суть | База / зависимости | Ревью | Следующий шаг |
|---|---|---|---|---|
| **#12426** | Matrix edited_messages (FR-9): дроп m.replace по умолчанию, opt-in доставка m.new_content с ChannelMessage.edited | `origin/master`, самодостаточен | **tidux CHANGES_REQUESTED 16.11 — БЛОКЕР ЗАКРЫТ 22.11** (`818693578c`, виза Ram, волна 3): edit-resolution против оригинала (fetch+валидация MSC2676: sender/type/redacted/edit-of-edit), оверлей new_content↔оригинальные relations (thread/in_reply_to из оригинала, m.mentions из new_content), timestamp=origin_server_ts, freshness 600с (как у telegram-политики) → дроп до approval-парсера; Malformed-класс (дроп в обоих режимах); доки сужены (edited=метаданные до оркестраторной edit-политики из #12371); body-совместимость исправлена (поведенческое изменение в обоих режимах); честная коррекция коммента 16.11 про media/caption. Разбор ВСЕХ находок + план конвергенции с #12371/#12439 — в ответе tidux. **МИКРОВОЛНА 23.11** (`f4ab0758b5`, виза Ram «fix them»): мастер влил #12495 (auto-accept инвайтов) — семантический конфликт в matrix.rs разрешён объединением (invite-тесты с invite_ctx + edit-тесты с test_ctx, все разрезанные конфликтом тела закрыты явно), em-dash в matrix.md заменены (Docs Style), hygiene-хвост `#12371` из сташа закоммичен; channels lib 2018/0; пуш + 2 комментария в PR (атрибуция инвайт-кода — #12495, исправлена) | ждём re-review tidux |
| **#12442** | Matrix owner-нотис (FR-10): owner_user_id, DM владельцу при allowlist-дропе, троттлинг 5 мин, m.direct, без эхо-гарда | `origin/master`, самодостаточен; Related #12346 | **ВНЕДРЁН В PROD NB 15.11** (fd5645f5, nb-base 609bcc0e3a); NB подтвердили приёмку, эпик 77 закрыт у обоих контуров; тесты 263/263 matrix:: | **tidux CHANGES_REQUESTED 16.11**: 🔴 E0063 + 🔴 троттл — **ОБА ЗАКРЫТЫ 22.11** (`373baca40f`, виза Ram, волна 2): 15 литералов дополнены `owner_user_id`, check all-targets чистый; троттл стал двухуровневым (per-пара + агрегат per-alias в той же мапе, несблокируемый ключ); верификация: matrix 264/0 (channel-matrix), config 4/0, fmt+hygiene; ребейз на свежий мастер в том же хэде; ответ tidux в PR. **МИКРОВОЛНА 23.11** (`ab3e7042bc`, виза Ram «fix them»): конфликт с #12495 (invite auto-accept) в matrix.rs разрешён объединением (invite-гейт + owner-notice flow), 3 em-dash в matrix.md заменены (Docs Style); check channels all-targets чист (2 dead_code-warning'а в platform-файлах config — предсуществующие, не наши); пуш + комментарий в PR | ждём re-review tidux |

## Issue (без PR)

| Issue | Суть | Следующий шаг |
|---|---|---|
| #12396 | keep_recent/collapse_tool_results — мёртвые knob'ы | vyahhi 14.11: strong overlap с #10781 (там accepted per-key disposition на более широкую поверхность); наш плюс — воспроизводимость на v0.8.7 и узкое предложение warn/removal. Похоже на путь к закрытию как дубликат | держать позицию «узкий warn/removal patch дополняет #10781»; при закрытии — не спорить, фиксировать вывод |

## План обслуживания (21.11)

Реестр ребейзов и кодовой работы по красным PR: `PR-MAINTENANCE-PLAN-2026-11-21.md`
(волна 1 — механические ребейзы, волна 2 — #12442/#12198, волна 3 — #12426/#12371).

**ВОЛНА 1 ИСПОЛНЕНА 21.11 (виза Ram):** #12549 → `91edc29463`, #12550 →
`3caa483ab3`, #12070 → `5ff374109b` (repl 111/0, prompt_cap 6/0), #12346 →
`df887e45c5` (channels 2045/5, все 5 — link_enricher-сеть), #11935 →
`8ce8780ea7` (cron 333/0, root bin compiles). Все пуши `--force-with-lease` с
комментариями; верификации и fmt/hygiene чистые. CI-трек у демиурга снят
(джоб a13ad43f удалён), наблюдение — крон сессии.

## Политика контура (виза Ram 16.11)

Форк ведём в соответствии с `origin/master`: регулярная синхронизация
`origin/master` → `local/fork-main` (первая — 16.11, merge `c38c4fe667`).
Апстриму предлагаем PR'ами, но не давим: никаких ускоряющих действий
(расплетение стеков, эскалации, повторные пинги чаще разумного) без
отдельной визы. Approved-but-not-merged — штатное состояние, ждём мейнтейнеров.

## Дисциплина пачки

1. Один PR — одна забота. Caption — только в #12404; voice-инфра — только в #11601.
2. Перед пушем в любую PR-ветку: `cargo test -p zeroclaw-channels --lib repl`
   (или шире при рантайм-изменениях) на ветке PR.
3. Любое изменение здесь — сразу после действия, не «потом».
4. Ребейз чужого ревью-стека: force-push только с `--force-with-lease` +
   комментарий в PR о том, что пересобрано.
