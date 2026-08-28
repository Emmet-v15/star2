#!/usr/bin/env bash
# Summarise live call telemetry per peer, straight from the rendezvous server.
#
#   ./deploy/measure.sh [SECONDS] [LABEL]
#
# Reports median buf/out (medians, because the adaptive targets step between two
# values and a mean would invent a figure neither peer ever used) alongside mean
# and worst-case loss. Worst-case matters more than the mean here: a call is
# judged by its dropouts, not its average.
#
# The latency block is measured, not modelled. Every column is a duration timed
# on ONE machine's clock, so no cross-machine clock comparison is involved:
#
#   devin  cpal callback-minus-capture, as reported by the host audio API
#   ring   input ring occupancy at the moment the encoder took a frame
#   enc    encode + send_to, measured around the call
#   rtt    probe->ack on the live media socket, includes peer turnaround
#   jb     packet arrival in recv_loop -> the moment playout rendered it
#   out    output ring occupancy (FIFO dwell for a frame entering now)
#   devout cpal playback-minus-callback, as reported by the host audio API
#
# txpath = devin+ring+enc, rxpath = jb+out+devout. The send and receive chains
# are each gap-free and non-overlapping, so the sums are meaningful.
#
# End-to-end mouth-to-ear for A talking to B is bounded by:
#     A.txpath + B.rxpath  <=  e2e  <=  A.txpath + B.rxpath + rtt
# The one-way network delay is somewhere inside rtt; splitting it would need
# synchronised clocks or an assumption of path symmetry, so we don't.
set -euo pipefail

WINDOW="${1:-60}"
LABEL="${2:-}"
HOST="${STAR2_HOST:-empire}"

ssh "$HOST" "journalctl -u star2-rendezvous --since '-${WINDOW}s' --no-pager | grep '\[stats\]'" \
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

lat = {n: r for n, r in peers.items() if any('rtt' in x for x in r)}
if lat:
    print()
    print(f'--- measured latency (ms, mean over window) ---')
    print(f'{\"peer\":<18} {\"devin\":>6} {\"ring\":>6} {\"enc\":>6} {\"txpath\":>7} {\"rtt\":>7} {\"jb\":>6} {\"out\":>6} {\"devout\":>7} {\"rxpath\":>7} {\"contr\":>6}')
    agg = {}
    for name, rows in sorted(lat.items()):
        g = lambda k: st.mean([float(r[k]) for r in rows if k in r] or [0.0])
        agg[name] = (g('txpath'), g('rxpath'), g('rtt'))
        print(f'{name:<18} {g(\"devin\"):>6.1f} {g(\"ring\"):>6.2f} {g(\"enc\"):>6.2f} '
              f'{g(\"txpath\"):>7.1f} {g(\"rtt\"):>7.1f} {g(\"jb\"):>6.1f} {g(\"out\"):>6.1f} '
              f'{g(\"devout\"):>7.1f} {g(\"rxpath\"):>7.1f} {g(\"contract\"):>6.2f}')
    names = sorted(agg)
    if len(names) == 2:
        a, b = names
        print()
        for src, dst in ((a, b), (b, a)):
            lo = agg[src][0] + agg[dst][1]
            rtt = max(agg[src][2], agg[dst][2])
            print(f'{src} -> {dst}: mouth-to-ear between {lo:.1f} and {lo + rtt:.1f} ms'
                  f'   (measured {lo:.1f}, network leg within rtt {rtt:.1f})')
if not peers:
    print('(no stats in window - is a call up?)')
"
