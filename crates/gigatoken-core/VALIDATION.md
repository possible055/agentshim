# Validation record

Measurements below were captured on Windows x64 on 2026-08-11 with release
builds. The benchmark cases are ignored in ordinary test runs and have no
production dependencies.

| Gate | Result |
|---|---:|
| Embedded model load p95 | 108.576 ms |
| Replacement 246,384-byte corpus first count | 3.048 ms |
| 0/25/50/75/100% overlap count p95 | 9.222/9.446/9.404/9.359/9.293 ms |
| Counter resident bytes after overlap cases | 142,208 bytes |
| 1/4/8/16 concurrent exact fits, wall time | 0.577/0.837/0.998/1.797 ms |
| 1/4/8 KiB small-output p95 ratio to byte-only | 0.9585/0.7531/1.0001 |
| Five-second high-churn soak | 928,651 iterations; 142,520 peak and final resident bytes |

The 8–32 KiB ASCII, code, JSON, CJK, and emoji exact-interval matrix ranged
from 0.044 ms to 2.594 ms p95. CJK at 32 KiB was the slowest case.

## Post-integration audit

After removing allocation-only envelope serialization, avoiding duplicate text
projection copies, and applying the existing cancellation stride to cached
pretokens, the canonical warm release target measured:

| Gate | Result |
|---|---:|
| Embedded model load p95 | 116.649 ms |
| Replacement corpus first count | 3.450 ms |
| 0/25/50/75/100% overlap count p95 | 9.369/10.307/9.860/9.474/9.574 ms |
| 1/4/8/16 concurrent exact fits, wall time | 0.527/0.696/1.049/2.099 ms |
| 1/4/8 KiB small-output p95 ratio to byte-only | 1.0103/0.8837/0.9480 |

An isolated cold target directory produced materially slower Windows timings
while compiling and scanning new release artifacts. The canonical warm target
passed the release gates; absolute Windows timings remain environment-sensitive.

The resource-soak test defaults to 30 minutes and accepts
`AGENTSHIM_TOKEN_SOAK_SECONDS` for shorter development probes. Native Linux
x86_64 measurements and a full 30-minute process working-set run require their
respective validation environments; this Windows session does not claim those
external measurements.
