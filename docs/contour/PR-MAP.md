# PR-MAP — реестр апстрим-кампании (RAM-RAMII)

Единая точка истины по открытым PR в zeroclaw-labs/zeroclaw. Обновляется при
каждом движении по любому PR. Правило контура: новая фича/фикс — сначала
`local/fork-main` + живой тест, потом PR отдельной веткой от `origin/master`.

## Телеграм-кластер (reply-медиа и voice)

| PR | Суть | База / зависимости | Ревью | Следующий шаг |
|---|---|---|---|---|
| **#12404** | reply-документы агенту как файлы + caption | `origin/master`, самодостаточен | свежий, CI идёт | ждать мейнтейнеров; при замечаниях — фикс сюда |
| **#12405** | фото/видео в reply-context | Depends #12404 (draft) | — | снять draft после принятия #12404 |
| **#11601** | transcription retry + voice-drop announce; voice-инфраструктура (notify_voice_drop, VoiceDropReason, transient budget) | `origin/master` | CHANGES_REQUESTED (старое) | фундамент: довести до мерджа — от него зависит voice-линия |
| **#12197** | voice paths в reply context (captions ВЫНЕСЕНЫ в #12404) | пересборка: master + #11601 | последнее замечание закрыто в tip 5.11, ревью не перезапрошено | ребейз-ветка `fix/telegram-reply-voice-paths`, force-push в ветку PR, комментарий + re-request Audacity88 |

Владение caption: **только #12404**. В #12197 caption-код не дублировать.

## Смежные voice-PR (не в стеке, но учитывать при ревью)

| PR | Суть | Примечание |
|---|---|---|
| #12201 | voice transcription без mention в mention-only группах | opt-in фича, независима |
| #12198 | default_provider fallback для transcription | независима |
| #11651 | 429 retry_after flood gate | независима |

## Независимые PR (телеграм и рантайм)

| PR | Суть | Статус |
|---|---|---|
| #12371 | edited_message с latest-wins coalescing | OPEN |
| #12346 | owner-notice о неавторизованных | OPEN, CI зелёный |
| #12070 | skills-splice уважает prompt cap | CHANGES_REQUESTED, фиксы запушены, ждёт re-review |
| #12056 | identity-секции в head direct-path промптов | OPEN |
| #11910 | fail-closed budget системного промпта | OPEN |
| #11934 | agent_alias атрибуция cron/gateway | OPEN |
| #11935 | `cron runs` CLI | OPEN |
| #11983 | policy refusal с reason/remediation | OPEN |
| #12204 | web_search compiled-out warning | OPEN |

## Issue (без PR)

| Issue | Суть | Следующий шаг |
|---|---|---|
| #12396 | keep_recent/collapse_tool_results — мёртвые knob'ы | ждём реакции; по сигналу — PR с удалением или warn |

## Дисциплина пачки

1. Один PR — одна забота. Caption — только в #12404; voice-инфра — только в #11601.
2. Перед пушем в любую PR-ветку: `cargo test -p zeroclaw-channels --lib repl`
   (или шире при рантайм-изменениях) на ветке PR.
3. Любое изменение здесь — сразу после действия, не «потом».
4. Ребейз чужого ревью-стека: force-push только с `--force-with-lease` +
   комментарий в PR о том, что пересобрано.
