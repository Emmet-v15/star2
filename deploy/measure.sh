#!/usr/bin/env bash
# Summarise live call telemetry per peer, straight from the signal server.
#
#   ./deploy/measure.sh [SECONDS] [LABEL]
#
# Reports median buf/out (medians, because the adaptive targets step between two
# values and a mean would invent a figure neither peer ever used) alongside mean
# and worst-case loss. Worst-case matters more than the mean here: a call is
# judged by its dropouts, not its average.
set -euo pipefail

WINDOW="${1:-60}"
LABEL="${2:-}"
HOST="${STAR2_HOST:-empire}"

ssh "$HOST" "journalctl -u star2-signal --since '-${WINDOW}s' --no-pager | grep '\[stats\]'" \
    | python3 -c "
import sys, re, statistics as st

peers = {}
for line in sys.stdin:
    m = re.search(r'\[stats\] (\S+?)\(s\d+\) (.*)', line)
    if not m:
        continue
    name, rest = m.group(1), m.group(2)
    kv = dict(re.findall(r'(\w+)=(-?[\d.]+)', rest))
    if not kv:
        continue
    peers.setdefault(name, []).append(kv)

label = '''$LABEL'''
window = '''$WINDOW'''
print(f'=== {label or \"measurement\"}  ({window}s window) ===')
print(f'{\"peer\":<18} {\"n\":>4} {\"buf\":>6} {\"out\":>5} {\"loss avg\":>9} {\"loss max\":>9} {\"late\":>5} {\"exp\":>5} {\"jit\":>6} {\"tx\":>5} {\"rx\":>5}')
for name, rows in sorted(peers.items()):
    f = lambda k: [float(r[k]) for r in rows if k in r]
    loss, late, exp = f('loss'), f('late'), f('expand')
    print(f'{name:<18} {len(rows):>4} '
          f'{st.median(f(\"buf\")):>5.0f}m {st.median(f(\"out\")):>4.0f}m '
          f'{st.mean(loss):>8.2f}% {max(loss):>8.2f}% '
          f'{sum(late)/len(late):>5.1f} {sum(exp)/len(exp):>5.1f} '
          f'{st.mean(f(\"jitter\")):>5.1f}m {st.median(f(\"tx\")):>5.0f} {st.median(f(\"rx\")):>5.0f}')
if not peers:
    print('(no stats in window - is a call up?)')
"
