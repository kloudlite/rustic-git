#!/usr/bin/env python3
# close-run.py <tenant> <run_id>: close a killed run's row (state failed, "killed by the operator")
# and delete every object the run left behind — the same helpers deploy/dev/pod/watch.py uses when
# it kills a pm2 run, for a run that ran as a Job and was killed from the laptop.
import os, sys
tenant, run_id = sys.argv[1], sys.argv[2]
sys.argv = ["watch", "job", tenant, "/dev/null"]
src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "watch.py")).read()
exec(src.split("seen = 0")[0])  # defines token(), close_row(), cleanup() for `tenant`
close_row(run_id)  # noqa: F821
cleanup(run_id)  # noqa: F821
