# Placard — deployment

The appliance is a MeLE Cyber X1 running Debian 13 (trixie), provisioned by
Ansible, running the `placard` .deb from a GitHub Release. Nothing is
hand-configured on the box; if you're editing files over SSH, you're doing it
wrong (§12 of `DESIGN.md`).

## First-time provisioning

1. **Install Debian 13** from netinst USB: no desktop, SSH server enabled,
   hostname `placard-NN`, static IP or DHCP reservation, your SSH key for
   root. Kernel must be ≥ 6.12 (stock trixie is fine) — the N150 iGPU is not
   driven by older kernels.
2. **BIOS**: Auto Power On → enabled. Boot order → internal disk first.
3. **Inventory**: add the box to `ansible/inventory/hosts.yml`:
   ```yaml
   placard:
     hosts:
       placard-01:
         ansible_host: 192.168.10.50
         placard_version: "0.1.0"
         hdmi_rate: 50
   ```
   `hdmi_rate` sets both the kernel `video=` mode and `display.fps` in
   `config.toml`. They must match; the playbook is the only thing that writes
   either.
4. **Run the playbook**:
   ```
   cd ansible
   ansible-playbook -i inventory/hosts.yml site.yml
   ```
   It installs base packages and chrony, pins the kernel cmdline (1080p mode,
   quiet console), masks `getty@tty1`, arms the hardware watchdog, caps
   journald, downloads and installs the pinned .deb (checksum-verified),
   templates `/etc/placard/config.toml`, enables the service, and reboots if
   the cmdline changed.
5. **Verify**: the screen shows HOUSE CLOSED (the boot scene);
   `curl http://placard-01:8080/api/status` returns `"ntp_synced": true`.
6. **Pull the power.** Confirm the box comes back to the same scene with no
   operator action. This step is not optional.
7. `dd` the disk to a golden image; keep it with the spare box.

## Upgrading / downgrading

Bump (or lower) `placard_version` in the inventory, re-run the playbook. It
downloads that release, verifies the checksum, installs it, restarts the
service. Running the playbook with no changes reports zero changes.

## Changing canned messages

Edit `ansible/group_vars/placard.yml`, re-run the playbook. The config is
re-templated and the service restarted (≈1 s of black).

## Bench iteration (not for venues)

```
make linux-bin     # release build inside debian:trixie via Docker/podman
BOX=placard-01 make deploy-dev
```

`deploy-dev` scps the binary straight onto the box and restarts the service.
Anything that goes to a venue goes through a tagged release and Ansible.

## Releasing

```
make release       # tags v$(VERSION) from Cargo.toml, pushes the tag
```

CI builds in a trixie container, runs the full test suite including golden
images, and attaches `placard_<ver>_amd64.deb` + `.sha256` to a GitHub
Release. That artefact is what Ansible pins.

## Remaining hardware verification (M3–M5)

Not checkable without the box; do these on the bench before the first show:

- `systemctl status placard` shows `Type=notify` healthy; `kill -STOP` on the
  process gets it restarted by the watchdog within 15 s.
- Glyph edges on the real display show no alpha fringing (DESIGN.md §15); if
  they fringe, the fallback is `textrender` fed from `appsrc`.
- Power pull mid-countdown returns to the same countdown unattended within
  30 s.
- 24 h soak: zero restarts in the journal, CPU under 70 °C, no dropped-frame
  warnings from `kmssink`.
