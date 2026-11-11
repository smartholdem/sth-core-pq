# Running a PQ Node as a Service and Limiting Logs

## Option A (recommended): systemd + journald, log size ≤ 50 MB

```bash
# 1. unit file (adjust paths/user as needed)
sudo cp docs/systemd/sth-core.service /etc/systemd/system/sth-core.service
sudo systemctl daemon-reload
sudo systemctl enable --now sth-core

# 2. limit the journal specifically for sth-core: 50 MB, automatic rotation
sudo mkdir -p /etc/systemd/journald.conf.d
sudo tee /etc/systemd/journald.conf.d/sth-core.conf >/dev/null <<'EOF'
[Journal]
SystemMaxUse=50M
SystemMaxFileSize=10M
MaxRetentionSec=14day
EOF
sudo systemctl restart systemd-journald
```

Working with the node:
```bash
systemctl status sth-core
journalctl -u sth-core -f                      # live logs
journalctl -u sth-core --since "-1h" | grep -E "ERROR|WARN"
journalctl -u sth-core | grep -E "CHAIN STALLED|fork|REJECTED"
sudo systemctl restart sth-core
sudo systemctl stop sth-core && ./sth-core audit-all --fix && sudo systemctl start sth-core
```
The `SystemMaxUse` limit applies to the entire system journal; if there are other noisy services on the host, increase it to 200M - the node writes about 3–5 MB per day at INFO level.

## Option B: keep nohup - rotate with logrotate, file ≤ 50 MB

`nohup ... > nohup.out` does not itself enforce a file limit. Use logrotate for rotation (checked hourly, truncates when > 50 MB, 3 gzip archives, without restarting the node - `copytruncate`):
```bash
sudo tee /etc/logrotate.d/sth-core >/dev/null <<'EOF'
/home/mainnet/sth-core/nohup.out {
    size 50M
    rotate 3
    compress
    delaycompress
    missingok
    notifempty
    copytruncate
}
EOF
# logrotate runs once a day by default; for hourly checks:
sudo cp /etc/cron.daily/logrotate /etc/cron.hourly/logrotate 2>/dev/null || true
sudo logrotate -d /etc/logrotate.d/sth-core   # config check (dry run)
```
Starting with this option:
```bash
cd ~/sth-core && nohup ./sth-core run > nohup.out 2>&1 &
```
(`--db-path ./data` and `--p2p` are not required - they are already set in `node.yaml`.)

## Option C: without external tools - truncate on the fly

If logrotate is unavailable, write through `rotatelogs` (apache2-utils) or `split`:
```bash
nohup ./sth-core run 2>&1 | rotatelogs -n 3 -e node.log 50M > /dev/null &
```
`-n 3` keeps 3 files of 50 MB each in a rotating cycle.

## Less noise in the logs

`RUST_LOG=info,sth_core::p2p_legacy::follow=warn` in the unit's `Environment=` removes lines such as "Received new block ..." (one per block); errors, WARN, `CHAIN STALLED`, `Starting Round`, and `Forged new block` remain. Temporarily restore `info` when investigating incidents.
