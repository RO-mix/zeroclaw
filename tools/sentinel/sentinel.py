#!/usr/bin/env python3
"""Sentinel — правиловый watcher контура zeroclaw. Без LLM, без суждения.

Уровень 0 иерархии: детект -> alarm-файл -> будит Демиурга (zeroclaw cron once).
Исключения с прямым сообщением Ram: мертвая сцена (R2, сервис не Running) и
эскалация (правило стреляло 3 раза без разбора).

Режимы:
  normal  — alarms/ + будение + прямые сообщения по исключениям
  silent  — до SENTINEL_SILENT_UNTIL: детекты только в silent.log

Запуск: задача планировщика каждые 5 мин от SYSTEM.
"""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
import urllib.request
from datetime import datetime, timedelta
from pathlib import Path

BASE = Path(r"C:\Users\Homework\.zeroclaw")
SENTINEL_DIR = BASE / "sentinel"
ALARMS = SENTINEL_DIR / "alarms"
STATE = SENTINEL_DIR / "sentinel-state.json"
LOG = SENTINEL_DIR / "sentinel.log"
SILENT_LOG = SENTINEL_DIR / "silent.log"
TRACE = BASE / "data" / "state" / "runtime-trace.jsonl"
CRON_DB = BASE / "data" / "cron" / "jobs.db"
ENV = SENTINEL_DIR / "sentinel.env"
RAM_CHAT = "5807828470"
SILENT_UNTIL = datetime(2026, 10, 20)  # silent mode (Ram: 3 дня калибровки)

COOLDOWNS = {"R1": 2, "R3": 6, "R4": 1, "R5": 2, "R6": 6, "R7": 1}  # часов
ESCALATE_AFTER = 3


def log(msg):
    LOG.parent.mkdir(parents=True, exist_ok=True)
    with LOG.open("a", encoding="utf-8") as f:
        f.write(f"{datetime.now():%Y-%m-%d %H:%M:%S} {msg}\n")


def load_state():
    if STATE.exists():
        try:
            return json.loads(STATE.read_text(encoding="utf-8"))
        except Exception:
            return {}
    return {}


def save_state(st):
    STATE.write_text(json.dumps(st, ensure_ascii=False, indent=1), encoding="utf-8")


def trace_events(since_minutes):
    cutoff = time.time() - since_minutes * 60
    events = []
    if not TRACE.exists():
        return events
    with TRACE.open("r", encoding="utf-8", errors="replace") as f:
        for line in f:
            try:
                d = json.loads(line)
            except Exception:
                continue
            ts = d.get("@timestamp", "")
            try:
                t = datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
            except Exception:
                continue
            if t >= cutoff:
                events.append(d)
    return events


def cooldown_ok(st, key, hours):
    last = st.get(key, {}).get("last_fired")
    if not last:
        return True
    return datetime.now() - datetime.fromisoformat(last) > timedelta(hours=hours)


def raise_alarm(st, rule, subject, evidence, extra=None):
    key = f"{rule}:{subject}"
    rec = st.setdefault(key, {"count": 0, "last_fired": None})
    now = datetime.now()
    if not cooldown_ok(st, key, COOLDOWNS.get(rule, 2)):
        return
    silent = datetime.now() < SILENT_UNTIL
    rec["count"] += 1
    rec["last_fired"] = now.isoformat()
    if silent:
        with SILENT_LOG.open("a", encoding="utf-8") as f:
            f.write(json.dumps({"rule": rule, "subject": subject, "evidence": evidence[:500],
                                "count": rec["count"], "at": now.isoformat()}, ensure_ascii=False) + "\n")
        log(f"silent {rule}/{subject} count={rec['count']}")
        return
    alarm = {"rule": rule, "subject": subject, "evidence": evidence[:2000],
             "fired_at": now.isoformat(), "count": rec["count"]}
    if extra:
        alarm.update(extra)
    ALARMS.mkdir(parents=True, exist_ok=True)
    path = ALARMS / f"alarm-{rule}-{now:%Y%m%dT%H%M%S}.json"
    path.write_text(json.dumps(alarm, ensure_ascii=False, indent=1), encoding="utf-8")
    log(f"ALARM {rule}/{subject} -> {path.name}")
    if rec["count"] >= ESCALATE_AFTER:
        extra = extra or {}
        alias = extra.get("agent_alias") or subject
        what = extra.get("first_error") or evidence
        what = " ".join(str(what).split())[:300]
        when = f"{now:%Y-%m-%d %H:%M}"
        tg_send(
            f"АЛАРМ: {rule} | Агент: {alias} | Когда: {when} | "
            f"Что случилось: {what} | "
            f"Почему эскалация: правило сработало {rec['count']} раз, "
            f"тревогу никто не разобрал; детали и evidence: "
            f"C:/Users/Homework/.zeroclaw/sentinel/alarms/{path.name}"
        )
        rec["count"] = 0
    wake_demiurge()


ALARM_PROMPT = (
    "SENTINEL-WAKE: обработай тревогу сентинеля. "
    "(1) Прочитай новейший файл в C:/Users/Homework/.zeroclaw/sentinel/alarms/ "
    "(glob_search, отсортируй по имени); если пусто — заверни ход строкой 'тихо'. "
    "(2) Верифицируй evidence своими инструментами. "
    "(3) Подтверждённая тревога: сводка Ram (chat_id=5807828470, channel_alias=demiurge) "
    "по вериф-гейту с типами [факт]/[вывод]/[гипотеза]. "
    "(4) Запись: контур → BODY/INTERVENTIONS.md, агент → BODY/SHADOW.md. "
    "(5) Файл alarm перенеси в C:/Users/Homework/.zeroclaw/sentinel/processed/. "
    "(6) Ложная тревога: пометка 'ложная' в processed/ + предложение отцу ослабить правило."
)


def wake_demiurge():
    try:
        exe = BASE / "zeroclaw.exe"
        subprocess.run([str(exe), "cron", "once", "--agent", "demiurge",
                        "--prompt", "1m", ALARM_PROMPT], timeout=120, capture_output=True)
        log("cron once (alarm) scheduled")
    except Exception as e:
        log(f"cron once failed: {e}")


def tg_send(text):
    try:
        token = None
        if ENV.exists():
            for ln in ENV.read_text(encoding="utf-8").splitlines():
                if ln.startswith("TELEGRAM_BOT_TOKEN="):
                    token = ln.split("=", 1)[1].strip()
        if not token:
            log("tg: no token")
            return
        proxy = urllib.request.ProxyHandler({"http": "http://127.0.0.1:10808",
                                             "https": "http://127.0.0.1:10808"})
        opener = urllib.request.build_opener(proxy)
        req = urllib.request.Request(
            f"https://api.telegram.org/bot{token}/sendMessage",
            data=json.dumps({"chat_id": RAM_CHAT, "text": text[:4000]}).encode(),
            headers={"Content-Type": "application/json"})
        opener.open(req, timeout=20)
        log("tg: sent")
    except Exception as e:
        log(f"tg failed: {e}")


def check_r1_r5(st):
    events = trace_events(15)
    per_agent = {}
    rate_limit = 0
    agent_alias = ""
    first_error = ""
    samples = []  # сырые события для alarm JSON — evidence не сгорает с ротацией трейса
    results = []   # tool_call_result failure — каноническая запись вызова
    raw_errors = []  # 'tool error'/'tool failed' — дублируют result либо фиксируют dispatch-отказ
    for e in events:
        msg = str(e.get("message", "")).lower()
        ev = e.get("event", {}) or {}
        attrs = e.get("attributes", {}) or {}
        if "rate" in msg and ("limit" in msg or "429" in msg) or "429" in str(attrs.get("error", "")):
            rate_limit += 1
        # R1: tool-ошибка — строго по outcome=failure от рантайма
        # (event.category="tool", event.outcome="failure" на tool_call_result).
        # stderr/warnings при успешном exit-коде (outcome=success, error_reason
        # пустой или заполненный текстом warnings) НЕ считаем ошибкой.
        if ev.get("category") != "tool" or ev.get("outcome") != "failure":
            continue
        err_text = str(attrs.get("error_reason") or attrs.get("error") or "")
        out_text = str(attrs.get("output") or "")
        # (б) калибровка 29.10: опрос фоновой задачи (check_result), вернувший
        # валидный JSON со status=running/finished и error=null, записывается
        # событием 'tool failed' с пустым error — опрос работающей задачи не
        # есть провал, в счётчик не включаем.
        if not err_text.strip() and out_text.strip():
            try:
                parsed = json.loads(out_text)
                if isinstance(parsed, dict) and parsed.get("status") in ("running", "finished") \
                        and parsed.get("error") in (None, ""):
                    continue
            except (ValueError, TypeError):
                pass
        if "tool_call_result" in msg:
            results.append((e, attrs, err_text))
        else:
            raw_errors.append((e, attrs, err_text))
    # (а) калибровка 29.10 (дедуп предложен 28.10): пара 'tool error' +
    # 'tool_call_result' с одним error_reason считалась двойно. Result —
    # каноническая запись; raw error включаем только если dispatch-отказ
    # не дал пары result (реальный отказ валидации вызова).
    counted = list(results)
    for e, attrs, err_text in raw_errors:
        tool = attrs.get("tool")
        minute = str(e.get("@timestamp", ""))[:16]
        twin = False
        for e2, attrs2, err2 in results:
            if attrs2.get("tool") != tool:
                continue
            if str(e2.get("@timestamp", ""))[:16] != minute:
                continue
            hay = err2 + " " + str(attrs2.get("output", ""))
            if not err_text or err_text in hay or (err2 and err2 in err_text):
                twin = True
                break
        if not twin:
            counted.append((e, attrs, err_text))
    for e, attrs, err_text in counted:
        ev_alias = (e.get("zeroclaw", {}) or {}).get("agent_alias") or ""
        if not agent_alias and ev_alias:
            agent_alias = ev_alias
        alias = ev_alias or attrs.get("tool", "?")
        per_agent[alias] = per_agent.get(alias, 0) + 1
        if not first_error and err_text:
            first_error = err_text[:200]
        if len(samples) < 3:
            trimmed = {k: (str(v)[:500]) for k, v in attrs.items()}
            samples.append({"timestamp": e.get("@timestamp", ""),
                            "id": e.get("id", ""),
                            "agent_alias": ev_alias,
                            "message": str(e.get("message", ""))[:200],
                            "attributes": trimmed})
    for alias, n in per_agent.items():
        if n >= 3:
            subj = agent_alias or alias
            raise_alarm(st, "R1", subj, f"{n} tool-ошибок за 15 мин",
                        {"agent_alias": agent_alias, "first_error": first_error,
                         "events": samples})
    if rate_limit >= 10:
        raise_alarm(st, "R5", "llm", f"{rate_limit} rate-limit ошибок за 15 мин")


def check_r2():
    try:
        out = subprocess.run(["sc", "query", "zeroclaw"], capture_output=True, text=True, timeout=15)
        if "RUNNING" not in out.stdout:
            raise_alarm({}, "R2", "zeroclaw-service", out.stdout.strip()[:500])
    except Exception as e:
        log(f"r2 check failed: {e}")


def check_r3(st):
    now = datetime.now()
    if not (now.hour == 7 and now.minute < 31):
        return
    today = now.strftime("%Y-%m-%d")
    try:
        conn = sqlite3.connect(f"file:{CRON_DB}?mode=ro", uri=True, timeout=5)
        try:
            row = conn.execute(
                "SELECT COUNT(*) FROM cron_runs r JOIN cron_jobs j ON j.id = r.job_id "
                "WHERE r.status = 'ok' AND j.prompt LIKE '%rss-digest%' "
                "AND substr(r.started_at, 1, 10) = ?",
                (today,),
            ).fetchone()
        finally:
            conn.close()
    except Exception as e:
        log(f"r3: cron db недоступна ({e}), правило пропущено")
        return
    if not row or row[0] == 0:
        raise_alarm(st, "R3", "digest", f"к {now:%H:%M} нет успешного прогона дайджеста (cron_runs)")


def check_r4(st):
    try:
        with urllib.request.urlopen("http://127.0.0.1:7777/health", timeout=5) as r:
            body = json.loads(r.read().decode())
            if body.get("status") != "ok":
                raise_alarm(st, "R4", "hub", f"health status={body.get('status')}")
    except Exception as e:
        raise_alarm(st, "R4", "hub", f"unreachable: {e}")


def check_r6(st):
    free = shutil.disk_usage("C:\\").free
    if free < 5 * 1024 ** 3:
        raise_alarm(st, "R6", "disk-C", f"свободно {free / 1024**3:.1f} ГБ (< 5)")


def check_r7(st):
    # father-bridge: заявка в inbox старше 30 минут без ответа в outbox —
    # father не исполняет (пейн мёртв, herdr server лёг, квота).
    bridge = BASE / "agents" / "demiurge" / "workspace" / "BRIDGE"
    inbox, outbox = bridge / "inbox", bridge / "outbox"
    if not inbox.is_dir():
        return
    now = time.time()
    for req in inbox.glob("*.json"):
        if req.name.startswith("."):
            continue
        if (outbox / req.name).exists() or (outbox / f"{req.stem}.question.json").exists() \
                or (outbox / f"{req.stem}.approval.json").exists():
            continue
        age_min = (now - req.stat().st_mtime) / 60
        if age_min > 30:
            raise_alarm(st, "R7", "father-bridge",
                        f"{req.name} висит {age_min:.0f} мин без outbox")
            return


def main():
    st = load_state()
    check_r2()          # мертвая сцена — вне кулдауна, всегда
    check_r1_r5(st)
    check_r3(st)
    check_r4(st)
    check_r6(st)
    check_r7(st)
    save_state(st)


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        ALARMS.mkdir(parents=True, exist_ok=True)
        (ALARMS / "alarm-TEST-selftest.json").write_text(
            json.dumps({"rule": "TEST", "subject": "selftest", "evidence": "ручной прогон",
                        "fired_at": datetime.now().isoformat(), "count": 1}, ensure_ascii=False))
        print("selftest alarm written")
    else:
        main()
