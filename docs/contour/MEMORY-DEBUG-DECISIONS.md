# Debug memory — операторская заметка по фильтрации трейса (9-17)

Слой наблюдаемости решений памяти: при `[memory] debug_decisions = true` пять
подсистем памяти эмитят `record!`-события на INFO со stable error_key
`memory.*` и фиксированными скалярными атрибутами (контент записей и тексты
запросов не эмитятся — только key/namespace/counts/decision/reason).

При выключенном флаге (дефолт) слой даёт ноль добавки к трейсу и hot path.

## Ключи

| error_key | Подсистема | Точка | Ключевые атрибуты |
|---|---|---|---|
| `memory.proposal_verdict` | proposals | `apply_signed` (после commit), `expire_stale` | decision=accepted\|rejected\|expired, source=ratification\|expiry, keys_count, has_reason |
| `memory.gate_disposition` | policy gate | `ScannedMemory::enforce_policy` (allow и deny) | decision=allow\|deny, violation_kind (deny), namespace, key |
| `memory.recall_selection` | retrieval | `RetrievalPipeline::cached_recall` (layer=pipeline) и `SqliteMemory::recall_scoped` / `recall_by_time_only` (layer=backend) | layer, path=fts\|vector\|hybrid\|like\|time_only, query_terms, candidates, returned, rejected_window, rejected_agent_scope, rejected_superseded, cache=hit\|miss\|off |
| `memory.hygiene_decision` | hygiene | `run_if_due` retention-ветки | action=archive\|prune\|compact\|skip, category, rows_affected, reason=retention\|cap |
| `memory.consolidation_decision` | consolidation | `consolidate_turn` | action=conflict_supersede\|proposal_queued\|signals_recorded\|typed_facts_stored, kind, key, outcome=ok\|skipped\|disabled |

## Ключевой отладочный кейс: recall не поднял запись

Событие backend-слоя показывает виновника пустой выдачи:
`candidates > 0, returned = 0` — запись нашлась до фильтров; какой из
счётчиков `rejected_*` ненулевой, тот фильтр и отсёк
(`rejected_agent_scope` — session/agent фильтр, `rejected_window` — since/until,
`rejected_superseded` — superseded/principal SQL-фильтр). При `path=like`
`candidates=0` по построению — смотреть `returned`.

## Фильтрация трейса (python one-liner)

Трейс демона — JSONL (runtime-trace.jsonl). Фильтр по error_key:

```bash
python -c "import json,sys;[print(l,end='') for l in open(sys.argv[1],encoding='utf-8') if (lambda e: e.get('attributes',{}).get('error_key','').startswith('memory.'))(json.loads(l))]" runtime-trace.jsonl
```

Сводка по ключам за день:

```bash
python -c "
import json,sys,collections
c=collections.Counter()
for l in open(sys.argv[1],encoding='utf-8'):
    try: k=json.loads(l).get('attributes',{}).get('error_key','')
    except Exception: continue
    if k.startswith('memory.'): c[k]+=1
print(c)" runtime-trace.jsonl
```

Разрез «пустой recall» за период (недельный memory review Демиурга):

```bash
python -c "
import json,sys
for l in open(sys.argv[1],encoding='utf-8'):
    try: e=json.loads(l)
    except Exception: continue
    a=e.get('attributes',{})
    if a.get('error_key')=='memory.recall_selection' and a.get('layer')=='backend' and a.get('returned')==0 and a.get('candidates',0)>0:
        print(a.get('path'), 'win=',a.get('rejected_window'), 'scope=',a.get('rejected_agent_scope'), 'sup=',a.get('rejected_superseded'))" runtime-trace.jsonl
```

## Контур наблюдения

- Ежедневный лёгкий чек — п.2в ночного обхода Демиурга (крон `58825a5e`):
  сводка счётчиков `memory.*` за сутки в отчёт.
- Еженедельный memory review — python-агрегация по трейсу (без LLM),
  сверка со `zeroclaw memory stats`, отчёт BODY/REPORTS/memory-<date>.md.
