# FreeBSD release drills, candidate 3befcfc (rc.d)

Host: immortal-freebsd, FreeBSD 15.1-RELEASE-p5 amd64. Run 2026-11-16 06:10-06:15 UTC, after the 12 benchmark runs finished (01:13 UTC). Each step is recorded by step.sh: NN-name.log holds the commands, output and exit status; steps/ holds the scripts.

| Step | Result |
|---|---|
| 00 build candidate (release) | pass |
| 02 candidate checks | pass |
| 04 app setup, 03 strict-v2 migration | pass (see note 1) |
| 05 dependency preview, 06 invalid-only | pass |
| 07 build previous (43a7a6e) | pass |
| 10 install previous via rc.d | pass: api and worker ready |
| 12 upgrade to candidate | pass: every supervisor runs the candidate binary |
| 13 shutdown (service stop) | pass |
| 14/15 reboot and boot via rc.d | pass: both services back about 2 s after boot, new PIDs |
| 17 rollback to 43a7a6e | pass: every supervisor runs the restored binary |
| 18 cleanup | pass (see note 2) |

Notes
1. 03 first failed because it needs the working directories 04 creates; the kit order was fixed (04 before 03). The failing first log was overwritten by the passing rerun.
2. The first 18 run removed everything, then its rc.conf check matched the hostname line (script false positive, fixed). The rerun failed at the first copy because the state was already gone (18-cleanup-rerun-not-idempotent.log); 18b-cleanup-verify.log re-ran the verification half and passes: no Immortal process, rc.d script, rc.conf setting, binary, definition, runtime or drill file remains.
3. At shutdown the services stopped abruptly (last heartbeat at the shutdown time, no ordered stop); the FreeBSD ordered-shutdown design is still open.
