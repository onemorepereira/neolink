"""PLAN 0.4 analysis: arrival skew between audio and video, and clock drift.

err(t) = audio position - video media time, sampled at each video arrival (DESIGN 4.2),
with audio anchored at the first AU to the latest video frame (skew_estimate = 0).
Clock drift = slope of err (median per 20 s window, start-up windows skipped), and of
the video arrival lower envelope vs host time over steady windows. Intervals are
widened for the lag-1 autocorrelation of the residuals.
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

def fit_ar1(xs, ys):
    """Slope with a 95% interval widened for lag-1 autocorrelation of the residuals"""
    b, se = fit(xs, ys)
    n = len(xs); mx = sum(xs)/n; my = sum(ys)/n
    res = [y - (my + b*(x-mx)) for x, y in zip(xs, ys)]
    den = sum(r*r for r in res)
    r1 = sum(res[i]*res[i-1] for i in range(1, n)) / den if den else 0.0
    r1 = max(0.0, r1)
    n_eff = n * (1 - r1) / (1 + r1)
    widen = (n / n_eff) ** 0.5 if n_eff > 2 else float('inf')
    return b, se * widen, r1, n_eff

SKIP = 6  # windows of start-up transient (backlog drain), 2 minutes
b, se, r1, n_eff = fit_ar1(wx[SKIP:], wy[SKIP:])
print(f"audio-vs-video drift: {b*1e3:.1f} ppm ± {1.96*se*1e3:.1f} (95%, lag-1 r={r1:.2f}, n_eff={n_eff:.0f}), i.e. {b*600:.0f} ms per 10 min")
# Host vs camera video clock: slope of the per-window minimum arrival offset, using only
# steady windows (minimum lag < 1 s), which excludes the backlog drain and congestion
win = {}
for h, o in venv: win.setdefault(h // W, []).append(o)
pts = [((k + 0.5) * W / 1e6, min(win[k]) / 1e3) for k in sorted(win) if min(win[k]) < 1_000_000]
b, se, r1, n_eff = fit_ar1([p[0] for p in pts], [p[1] for p in pts])
print(f"host-vs-camera video clock: {b*1e3:.1f} ppm ± {1.96*se*1e3:.1f} (95%, {len(pts)} steady windows, n_eff={n_eff:.0f})")
# Inter-arrival gaps
g = sorted((venv[i][0]-venv[i-1][0])/1e3 for i in range(1, len(venv)))
print(f"video inter-arrival ms: p50 {g[len(g)//2]:.0f} p99 {g[int(len(g)*.99)]:.0f} max {g[-1]:.0f}")
