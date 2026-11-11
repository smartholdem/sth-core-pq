# Запуск PQ Node как сервиса и ограничение логов

## Вариант A (рекомендуется): systemd + journald, журнал ≤ 50 МБ

```bash
# 1. юнит (пути/пользователь - поправить под себя)
sudo cp docs/systemd/sth-core.service /etc/systemd/system/sth-core.service
sudo systemctl daemon-reload
sudo systemctl enable --now sth-core

# 2. лимит журнала именно для sth-core: 50 МБ, ротация автоматическая
sudo mkdir -p /etc/systemd/journald.conf.d
sudo tee /etc/systemd/journald.conf.d/sth-core.conf >/dev/null <<'EOF'
[Journal]
SystemMaxUse=50M
SystemMaxFileSize=10M
MaxRetentionSec=14day
EOF
sudo systemctl restart systemd-journald
```

Работа с нодой:
```bash
systemctl status sth-core
journalctl -u sth-core -f                      # живой лог
journalctl -u sth-core --since "-1h" | grep -E "ERROR|WARN"
journalctl -u sth-core | grep -E "CHAIN STALLED|fork|REJECTED"
sudo systemctl restart sth-core
sudo systemctl stop sth-core && ./sth-core audit-all --fix && sudo systemctl start sth-core
```
Ограничение `SystemMaxUse` действует на весь системный журнал; если на хосте другие шумные сервисы, поднимите до 200M -
нода пишет ≈ 3–5 МБ в сутки на INFO.

## Вариант B: остаётся nohup - ротация logrotate, файл ≤ 50 МБ

`nohup ... > nohup.out` сам файл не ограничивает. Ротация через logrotate (проверка каждый час, обрезка при > 50 МБ,
3 архива в gzip, без перезапуска ноды - `copytruncate`):
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
# logrotate по умолчанию запускается раз в сутки; для почасовой проверки:
sudo cp /etc/cron.daily/logrotate /etc/cron.hourly/logrotate 2>/dev/null || true
sudo logrotate -d /etc/logrotate.d/sth-core   # проверка конфига (dry run)
```
Запуск при этом варианте:
```bash
cd ~/sth-core && nohup ./sth-core run > nohup.out 2>&1 &
```
(`--db-path ./data` и `--p2p` не нужны - они уже заданы в `node.yaml`.)

## Вариант C: без внешних инструментов - обрезать "на лету"

Если logrotate недоступен, писать через `rotatelogs` (apache2-utils) или `split`:
```bash
nohup ./sth-core run 2>&1 | rotatelogs -n 3 -e node.log 50M > /dev/null &
```
`-n 3` - хранить 3 файла по 50 МБ по кругу.

## Меньше шума в логе

`RUST_LOG=info,sth_core::p2p_legacy::follow=warn` в `Environment=` юнита убирает строки "Received new block ..." (по одной на
каждый блок); ошибки, WARN, `CHAIN STALLED`, `Starting Round`, `Forged new block` остаются. Для разбора инцидентов
временно верните `info`.
