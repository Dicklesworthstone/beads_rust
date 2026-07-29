# doctor_runs_dir_growth

- **FM**: `fm-observability-doctor-runs-dir-grows-unbounded` (P2)
- **Detector**: `doctor.runs_dir`
- **Subsystem**: observability
- **Shape**: a workspace accumulates more than 50 historical
  `.doctor/runs/<run-id>/` directories.
- **Repair contract**: detect-only. Doctor must not prune its own audit
  history during `--repair`; removing old run directories would break
  later `br doctor undo <run-id>` calls and destroy recovery evidence.
- **Round-trip**: create 55 synthetic run directories -> detect warns ->
  `--repair` leaves the existing run directories intact -> undo leaves the
  audit history intact.

