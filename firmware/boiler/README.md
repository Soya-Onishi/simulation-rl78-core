# boiler firmware (RL78 guest)

Water-heater control program for `boiler_board` (`kind=rl78boiler`).
Startup / UART / TAU patterns follow [rl78-uart-sample](https://github.com/Soya-Onishi/rl78-uart-sample).

## Build

Requires Renesas CC-RL (`ccrl` on `PATH`):

```bash
make build   # produces main.abs (ELF)
```

Override toolchain paths if needed:

```bash
make build CCRL=/path/to/ccrl CCRL_LIB=/path/to/rl78em4r.lib
```

## Behavior (design lock)

| Item | Behavior |
|------|----------|
| P0.0 | Flow pulse input; Lo→Hi edges counted |
| P0.1 / P0.2 | `temp_up` / `temp_down`; rising edge ±1 °C (40–50) |
| ANI0 / ANI1 | Inlet / outlet thermistor, alternate 250 µs, 10-bit → 0.1 °C |
| UART0 TX | Heat command 0–100 every 10 ms (on change), 9600 8N1 |
| TAU0 tick | `TPS0=0x0067` (CK0=/128), `TDR00=62` → `(TDR+1)*128/32e6` ≈ 252 µs |
| Flow GPIO sample | Every TAU0 tick (~250 µs) |
| Flow | Every 100 ms (400 ticks): `flow_01 = min(250, N*5)` (0.1 L/min) |
| Heat cmd | `cmd = min(100, (60*Q*dT)/15000)` with Q,dT in 0.1 units |

Load `main.abs` via the cluster topology `elf` field on the `rl78boiler` board.
