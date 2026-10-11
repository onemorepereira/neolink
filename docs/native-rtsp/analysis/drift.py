"""PLAN 0.4 analysis: arrival skew between audio and video, and clock drift.

err(t) = audio position - video media time, sampled at each video arrival (DESIGN 4.2),
with audio anchored at the first AU to the latest video frame (skew_estimate = 0).
Clock drift = slope of err (median per 20 s window), and of the video arrival lower
envelope vs host time.
"""
import struct, sys, statistics as st

def read(p):
    d = open(p, 'rb').read(); i = 0; out = []
    while i < len(d):
        h, k, cu, ct, l = struct.unpack_from('<QBIII', d, i); i += 21
        out.append((h, k & 0x7f, cu)); i += l
    return out

def fit(xs, ys):
    n = len(xs); mx = sum(xs)/n; my = sum(ys)/n
    sxx = sum((x-mx)**2 for x in xs); sxy = sum((x-mx)*(y-my) for x, y in zip(xs, ys))
    b = sxy/sxx; a = my - b*mx
    res = [y-(a+b*x) for x, y in zip(xs, ys)]
    se = (sum(r*r for r in res)/(n-2)/sxx) ** 0.5 if n > 2 else float('nan')
    return b, se

recs = read(sys.argv[1])
if any(k == 0x7f for _, k, _ in recs):
    recs = recs[:[k for _, k, _ in recs].index(0x7f)]
video_t = None; media = 0; last_cam = None; audio_n = 0; anchor = None
errs = []; venv = []
for h, k, cu in recs:
    if k in (3, 4):
        if last_cam is not None:
            d = (cu - last_cam) & 0xffffffff
            media += d if d <= 2_000_000 else 66667
        last_cam = cu
        venv.append((h, h - media))
        if anchor is not None:
            a_pos = anchor + (audio_n * 1024 * 1_000_000) // 16000
            errs.append((h, a_pos - media))
    elif k == 5:
        if anchor is None and last_cam is not None:
            anchor = media
            audio_n = 0
        audio_n += 1 if anchor is not None else 0
dur = recs[-1][0] / 1e6
print(f"duration {dur:.0f} s, video {len(venv)}, err samples {len(errs)}")
e = [x for _, x in errs]
q = st.quantiles(e, n=20)
print(f"err (audio - video) ms: median {st.median(e)/1e3:.0f}  p5 {q[0]/1e3:.0f}  p95 {q[-1]/1e3:.0f}  min {min(e)/1e3:.0f}  max {max(e)/1e3:.0f}")
# 20 s windowed medians
W = 20_000_000
win = {}
for h, x in errs: win.setdefault(h // W, []).append(x)
wx = [(k*W + W/2)/1e6 for k in sorted(win)]; wy = [st.median(win[k])/1e3 for k in sorted(win)]
print("window medians (ms):", " ".join(f"{y:.0f}" for y in wy))
b, se = fit(wx[1:], wy[1:])  # skip the startup window
print(f"audio-vs-video drift: {b*1e3:.1f} ppm ± {1.96*se*1e3:.1f} (95%), i.e. {b*600:.0f} ms per 10 min")
# Host vs camera video clock: slope of per-window minimum arrival offset
win = {}
for h, o in venv: win.setdefault(h // W, []).append(o)
ks = sorted(win)[2:]  # skip the backlog drain
b, se = fit([k*W/1e6 for k in ks], [min(win[k])/1e3 for k in ks])
print(f"host-vs-camera video clock: {b*1e3:.1f} ppm ± {1.96*se*1e3:.1f} (95%)")
# Inter-arrival gaps
g = sorted((venv[i][0]-venv[i-1][0])/1e3 for i in range(1, len(venv)))
print(f"video inter-arrival ms: p50 {g[len(g)//2]:.0f} p99 {g[int(len(g)*.99)]:.0f} max {g[-1]:.0f}")
