/*
 * Water-heater control guest for boiler_board (kind=rl78boiler).
 *
 * Pin map (Q39): P0.0 flow pulse, P0.1 temp_up, P0.2 temp_down,
 * ANI0 inlet thermistor, ANI1 outlet thermistor, UART0 TX heat command.
 *
 * Timing (TAU0 ≈ 250 µs/tick; TPS0 same as rl78-uart-sample, TDR00=625):
 *   every 250 µs — sample flow GPIO + alternate ADC
 *   every 10 ms  — TX heat command byte (40 ticks)
 *   every 100 ms — convert Lo→Hi edge count to flow (0.1 L/min units; 400 ticks)
 */

#define INTST0  0x001E
#define INTTM00 0x0020

#define P0      (*(volatile unsigned char *)0xFF00)
#define PM0     (*(volatile unsigned char *)0xFF20)

#define SPS0    (*(volatile unsigned short *)0x0126)
#define SMR00   (*(volatile unsigned short *)0x0110)
#define SCR00   (*(volatile unsigned short *)0x0118)
#define SDR00   (*(volatile unsigned short *)0xFF10)
#define SOE0    (*(volatile unsigned short *)0x012A)
#define SS0     (*(volatile unsigned short *)0x0122)

#define TPS0    (*(volatile unsigned short *)0x01B6)
#define TMR00   (*(volatile unsigned short *)0x0190)
#define TDR00   (*(volatile unsigned short *)0xFF18)
#define TS0     (*(volatile unsigned short *)0x01B2)

#define ADM0    (*(volatile unsigned char *)0xFFF30)
#define ADS     (*(volatile unsigned char *)0xFFF31)
#define ADM1    (*(volatile unsigned char *)0xFFF32)
#define ADCR    (*(volatile unsigned short *)0xFFF1E)

#define MK0     (*(volatile unsigned short *)0xFFE4)

#ifdef __RL78__
#define EI() __EI()
#define DI() __DI()
#else
#define EI()
#define DI()
#endif

/* 1 tick = 250 µs (TDR00=625). */
#define FLOW_WINDOW_TICKS 400U /* 100 ms */
#define UART_PERIOD_TICKS 40U  /* 10 ms */
#define SETPOINT_MIN_01 400U /* 40.0 °C */
#define SETPOINT_MAX_01 500U /* 50.0 °C */
#define FLOW_MAX_01 250U     /* 25.0 L/min */

static volatile unsigned short timer_count;
static volatile unsigned char uart_busy;
static volatile unsigned char heat_cmd_byte;

static unsigned char flow_prev_hi;
static unsigned char flow_edges;
static unsigned char flow_edges_window;
static unsigned short flow_01; /* 0.1 L/min */
static unsigned short tin_01;  /* 0.1 °C */
static volatile unsigned short tout_01; /* 0.1 °C; kept for host/GDB inspect */
static unsigned short tset_01; /* 0.1 °C */
static unsigned char adc_next_outlet;
static unsigned char sw_up_prev;
static unsigned char sw_down_prev;

#pragma interrupt sau_tx_int(vect=INTST0)
void sau_tx_int(void) {
    uart_busy = 0;
}

#pragma interrupt tau_int(vect=INTTM00)
void tau_int(void) {
    timer_count++;
}

static void init_sau(void) {
    /* Same 9600 8N1 setup as rl78-uart-sample uart-tx. */
    SPS0 = 0x0003;
    SMR00 = 0x0022;
    SCR00 = 0x8093;
    SDR00 = 103U << 9;
    SOE0 |= 0x0001;
    SS0 |= 0x0001;
}

static void init_tau(void) {
    TPS0 = 0x0067;
    TMR00 = 0x0000;
    TDR00 = 625; /* ≈ 250 µs/tick (was 2500 ≈ 1 ms) */
    TS0 = 0x0001;
}

static void init_adc(void) {
    ADM1 = 0x20; /* 10-bit select mode used by the simulator tests */
    ADS = 0x00;
    ADM0 = 0x00;
}

static void init_gpio(void) {
    /* P0.0–P0.2 inputs: flow, temp_up, temp_down. */
    PM0 |= 0x07;
}

static unsigned short adcr_to_temp_01(unsigned short adcr) {
    unsigned long code = (unsigned long)(adcr >> 6) & 0x3FFUL;
    return (unsigned short)((code * 1000UL) / 1023UL);
}

static void start_adc(unsigned char channel) {
    ADS = channel & 0x1FU;
    ADM0 = 0x80; /* ADCS start */
}

static unsigned char adc_ready(void) {
    return (ADM0 & 0x80) == 0;
}

static void sample_flow_gpio(void) {
    unsigned char hi = (P0 & 0x01) != 0;
    if (hi && !flow_prev_hi) {
        if (flow_edges < 255U) {
            flow_edges++;
        }
    }
    flow_prev_hi = hi;
}

static void update_flow_from_window(void) {
    /* N edges / 100 ms; 50 edges ⇒ 25.0 L/min; store tenths: N * 5. */
    unsigned short q;
    flow_edges_window = flow_edges;
    flow_edges = 0;
    q = (unsigned short)flow_edges_window * 5U;
    if (q > FLOW_MAX_01) {
        q = FLOW_MAX_01;
    }
    flow_01 = q;
}

static unsigned char compute_heat_cmd(void) {
    unsigned long dt;
    unsigned long cmd;
    if (flow_01 == 0U || tset_01 <= tin_01) {
        return 0;
    }
    dt = (unsigned long)(tset_01 - tin_01);
    /* cmd = (60 * Q * dT) / 15000 with Q,dT in 0.1 units (= / (100*150)). */
    cmd = (60UL * (unsigned long)flow_01 * dt) / 15000UL;
    if (cmd > 100UL) {
        cmd = 100UL;
    }
    return (unsigned char)cmd;
}

static void poll_setpoint_switches(void) {
    unsigned char up = (P0 & 0x02) != 0;
    unsigned char down = (P0 & 0x04) != 0;
    if (up && !sw_up_prev) {
        if (tset_01 < SETPOINT_MAX_01) {
            tset_01 = (unsigned short)(tset_01 + 10U); /* +1.0 °C */
        }
    }
    if (down && !sw_down_prev) {
        if (tset_01 > SETPOINT_MIN_01) {
            tset_01 = (unsigned short)(tset_01 - 10U); /* -1.0 °C */
        }
    }
    sw_up_prev = up;
    sw_down_prev = down;
}

static void kick_uart_byte(unsigned char byte) {
    if (uart_busy) {
        return;
    }

    if(heat_cmd_byte != byte) {
        uart_busy = 1;
        heat_cmd_byte = byte;
        SDR00 = byte;
    }
}

int main(void) {
    unsigned short last_tick = 0;
    unsigned short tick_mod_window = 0;

    DI();
    timer_count = 0;
    uart_busy = 0;
    heat_cmd_byte = 0;
    flow_prev_hi = 0;
    flow_edges = 0;
    flow_edges_window = 0;
    flow_01 = 0;
    tin_01 = 200;  /* 20.0 °C until first ADC */
    tout_01 = 200;
    tset_01 = SETPOINT_MIN_01;
    adc_next_outlet = 0;
    sw_up_prev = 0;
    sw_down_prev = 0;

    init_gpio();
    init_sau();
    init_tau();
    init_adc();
    /* Unmask INTST0 and INTTM00 (same mask clear as uart-sample). */
    MK0 &= (unsigned short)~0x6000;
    EI();

    start_adc(0);

    while (1) {
        unsigned short now;
        unsigned short elapsed;

        DI();
        now = timer_count;
        EI();

        if (now == last_tick) {
            continue;
        }
        elapsed = (unsigned short)(now - last_tick);
        last_tick = now;

        while (elapsed > 0U) {
            elapsed--;
            tick_mod_window++;
            if (tick_mod_window >= FLOW_WINDOW_TICKS) {
                tick_mod_window = 0;
            }

            sample_flow_gpio();
            poll_setpoint_switches();

            if (adc_ready()) {
                unsigned short temp = adcr_to_temp_01(ADCR);
                if (adc_next_outlet) {
                    tout_01 = temp;
                    adc_next_outlet = 0;
                    start_adc(0); /* ANI0 inlet next */
                } else {
                    tin_01 = temp;
                    adc_next_outlet = 1;
                    start_adc(1); /* ANI1 outlet next */
                }
            }

            if ((tick_mod_window % UART_PERIOD_TICKS) == 0U) {
                kick_uart_byte(compute_heat_cmd());
            }
            if (tick_mod_window == 0U) {
                update_flow_from_window();
            }
        }
    }
}
