# Backup Throughput Baseline

What a healthy DAS backup run looks like, and — more importantly — **why
throughput is the wrong thing to watch for the fault that motivated this
document**.

Tracks `bd DAS-Backup-Manager-6lr`.

---

## 1. The finding, first

Between **2026-08-19 and 2026-08-28** the TerraMaster enclosure negotiated its
USB link at **480 Mbit/s** (USB 2.0 High Speed) instead of **10 000 Mbit/s**
(USB 3.2 Gen 2). For nine days:

- every backup completed,
- every backup reported success,
- and nothing anywhere detected the degradation.

The runs were merely slower. That is the whole failure: a fault that produces
**correct output more slowly** is invisible to every check that asks "did it
work?".

**Throughput did not detect it, and could not have.** The measured aggregate
write rates either side of the repair were:

| Date | USB link speed | Aggregate write rate |
|:-----------|:-------------------|:---------------------|
| 2026-08-28 | 480 Mbit/s | 9.17 MiB/s |
| 2026-08-29 | 10 000 Mbit/s | 10.97 MiB/s |

9.17 against 10.97 MiB/s is **inside ordinary run-to-run variation**. A
throughput trend line, a threshold alert, or an operator glancing at the report
would all have passed it. Only the **negotiated link speed** separates the two
states, and it does so unambiguously — 480 against 10 000 is not a trend, it is
a different number by a factor of about 21.

> **The rule this yields**: when a fault degrades a *rate* rather than a
> *result*, measure the thing that changes discontinuously (the negotiated link
> speed), not the thing that changes continuously (the observed throughput).
> The continuous signal is buried in noise; the discontinuous one is not.

---

## 2. Why link speed and transfer rate are only loosely coupled here

The enclosure is deliberately bound to `usb-storage` (Bulk-Only Transport)
rather than UAS, by `/etc/modprobe.d/terramaster-no-uas.conf`. BOT issues one
command at a time with no queuing, so once the link is above roughly USB 3.0
Gen 1 rates, the **spindles and the BOT protocol** are the limit — not the bus.

That quirk is a correct stability trade and **should stay**. It is also exactly
why a fast link does not translate into a proportionally fast backup, and why
the 2026-08-31 run reaching 19.80 MiB/s on the same 10 000 Mbit/s link as the
2026-08-29 run's 10.97 MiB/s is unremarkable: the variable is what the run had
to write and where it landed on the platters, not the bus.

So: **the link speed is a health check. The throughput is a capacity-planning
figure.** They answer different questions and neither substitutes for the other.

---

## 3. Measured baseline

All figures from `journalctl -u das-backup`. "Written" is bytes actually sent to
the backup target; "elapsed" is wall-clock for the whole run.

| Date | USB link speed | Elapsed (wall clock) | Written | Aggregate rate |
|:-----------|:-------------------|:---------------------|:-----------|:---------------|
| 2026-08-26 | 480 Mbit/s | 1 h 10 m 37 s | not recorded | not recorded |
| 2026-08-27 | 480 Mbit/s | 1 h 05 m 40 s | not recorded | not recorded |
| 2026-08-28 | 480 Mbit/s | 1 h 15 m 27 s | 40.52 GiB | 9.17 MiB/s |
| 2026-08-29 | 10 000 Mbit/s | 13 m 22 s | 8.59 GiB | 10.97 MiB/s |
| 2026-08-31 | 10 000 Mbit/s | 11 m 12 s | 13.00 GiB | 19.80 MiB/s |

**Reference datum**: the 2026-08-29 run — the first after the cable was
reseated — at **10.97 MiB/s** aggregate on a **10 000 Mbit/s** link.

Note that elapsed time tracks *how much there was to write*, not link health:
the 2026-08-28 run took an hour and a quarter because it wrote 40.52 GiB, which
is roughly five times the 2026-08-29 run's 8.59 GiB. Comparing wall-clock across
runs is meaningless without the byte count beside it, which is why both are
recorded together below.

---

## 4. What the system now records

Every backup appends one JSON object per run to
**`/var/lib/das-backup/throughput.jsonl`**:

```json
{"ts":"2026-08-31T03:11:12-05:00","elapsed_s":672,"bytes":13958643712,"bytes_per_s":20766623,"usb_link_mbit_s":"10000"}
```

| Field | Dimension | Meaning |
|:------------------|:------------------------|:------------------------------------|
| `ts` | timestamp (ISO 8601) | when the run finished |
| `elapsed_s` | seconds | wall-clock duration of the run |
| `bytes` | bytes | total written to the backup target |
| `bytes_per_s` | bytes per second | aggregate write rate |
| `usb_link_mbit_s` | megabits per second | negotiated USB link speed |

`usb_link_mbit_s` is a **string**, so a run where the enclosure could not be
identified records the literal `"unknown"` rather than a plausible-looking `0`.
A zero would be indistinguishable from a real measurement of zero; `"unknown"`
cannot be mistaken for data.

The line is appended to a file, one object per line, so the history is
greppable without a parser and a partial write can never corrupt earlier runs.

### The alert

`backup-run.sh` records an operation status of `FAIL` for `usb_link` — surfaced
as a row in the emailed report — whenever the negotiated link is **below
5000 Mbit/s**. The threshold sits deliberately between the two real states:
USB 3.0 Gen 1 is 5000 Mbit/s, Gen 2 is 10 000 Mbit/s, and USB 2.0 High Speed is
480 Mbit/s. Anything at or under Gen 1 rates on this enclosure means the link
did not come up as it should.

**The backup is not aborted.** A degraded link still produces a correct backup;
it just takes longer. Failing the run would trade a slow backup for no backup,
which is the wrong trade. The operator is told, and the run proceeds.

---

## 5. If the link is degraded

The fix is physical and takes about a minute.

1. **Power the enclosure down first.** Bay mapping and the shutdown procedure
   are in [`DAS-BAY-MAPPING.md`](DAS-BAY-MAPPING.md).
2. Reseat the USB-C cable at **both** ends — the enclosure and the host's native
   USB-C port.
3. Power the enclosure back up and confirm the link:

   ```bash
   for d in /sys/bus/usb/devices/*/; do
       [[ -r "$d/product" ]] || continue
       case "$(cat "$d/product")" in
           *TDAS*) echo "$(cat "$d/speed") Mbit/s";;
       esac
   done
   ```

   Every line must read **10000 Mbit/s**. Four lines are expected — one per bay.

4. Re-scan multi-device filesystems before mounting anything, because USB
   re-enumeration deregisters them:

   ```bash
   sudo btrfs device scan
   ```

Do **not** hand-mount the backup targets afterwards — `backup-run.sh` mounts
them itself by UUID, and its bare-mountpoint guard treats an absent target as a
valid state.

---

## 6. Related

- [`DAS-BAY-MAPPING.md`](DAS-BAY-MAPPING.md) — bay layout and safe power-down
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — where the report and its
  operation-status rows come from
- `.claude/rules/backup.md` — enclosure topology and the `no-uas` decision
