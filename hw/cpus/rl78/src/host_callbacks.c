/*
 * Strong tlib host callbacks. Compiled into one .o so that referencing
 * `rl78_tlib_callbacks_anchor` from Rust pulls every override out of the
 * archive (beating weak stubs inside libtlib.a).
 */
#include <stdint.h>
#include <stdlib.h>

void *rl78_host_guest_offset_to_host_ptr(uint64_t offset);
uint64_t rl78_host_read_byte(uint64_t address, uint64_t cpustate);
uint64_t rl78_host_read_word(uint64_t address, uint64_t cpustate);
uint64_t rl78_host_read_double_word(uint64_t address, uint64_t cpustate);
uint64_t rl78_host_read_quad_word(uint64_t address, uint64_t cpustate);
void rl78_host_write_byte(uint64_t address, uint64_t value, uint64_t cpustate);
void rl78_host_write_word(uint64_t address, uint64_t value, uint64_t cpustate);
void rl78_host_write_double_word(uint64_t address, uint64_t value, uint64_t cpustate);
void rl78_host_write_quad_word(uint64_t address, uint64_t value, uint64_t cpustate);
void rl78_host_abort(char *message);
void rl78_host_log(int32_t level, char *message);
void rl78_host_on_rl78_irq_ack(uint32_t index);

void rl78_tlib_callbacks_anchor(void) {}

void *tlib_guest_offset_to_host_ptr(uint64_t offset)
{
    return rl78_host_guest_offset_to_host_ptr(offset);
}

uint64_t tlib_read_byte(uint64_t address, uint64_t cpustate)
{
    return rl78_host_read_byte(address, cpustate);
}

uint64_t tlib_read_word(uint64_t address, uint64_t cpustate)
{
    return rl78_host_read_word(address, cpustate);
}

uint64_t tlib_read_double_word(uint64_t address, uint64_t cpustate)
{
    return rl78_host_read_double_word(address, cpustate);
}

uint64_t tlib_read_quad_word(uint64_t address, uint64_t cpustate)
{
    return rl78_host_read_quad_word(address, cpustate);
}

void tlib_write_byte(uint64_t address, uint64_t value, uint64_t cpustate)
{
    rl78_host_write_byte(address, value, cpustate);
}

void tlib_write_word(uint64_t address, uint64_t value, uint64_t cpustate)
{
    rl78_host_write_word(address, value, cpustate);
}

void tlib_write_double_word(uint64_t address, uint64_t value, uint64_t cpustate)
{
    rl78_host_write_double_word(address, value, cpustate);
}

void tlib_write_quad_word(uint64_t address, uint64_t value, uint64_t cpustate)
{
    rl78_host_write_quad_word(address, value, cpustate);
}

void tlib_abort(char *message)
{
    rl78_host_abort(message);
}

void tlib_log(int32_t level, char *message)
{
    rl78_host_log(level, message);
}

void tlib_on_rl78_irq_ack(uint32_t index)
{
    rl78_host_on_rl78_irq_ack(index);
}
