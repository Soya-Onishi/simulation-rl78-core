    .public _start
    .public _exit

.SECTION    .dataR, DATA
.SECTION    .sdataR, DATA

.SECTION    .stack_bss, BSS
_stackend:
    .DS 0x200
_stacktop:

_start  .VECTOR 0

.SECTION    .text, TEXT
_start:
    MOVW    SP, #LOWW(_stacktop)

    MOVW    AX,#LOWW(STARTOF(.ram_init_table))
    BR      $.L5_RAM_INIT_TABLE
.L1_RAM_INIT_TABLE:
    PUSH    AX              ;table pointer
    MOVW    HL,AX
    MOV     ES,#HIGHW(STARTOF(.ram_init_table))
    MOVW    AX,ES:[HL+6]    ;dst
    MOVW    DE,AX
    MOVW    AX,ES:[HL+4]    ;size
    ADDW    AX,DE
    MOVW    BC,AX           ;end
    MOV     A,ES:[HL+2]     ;high(src)
    CMP     A,#0xF
    BZ      $.L3_RAM_INIT_TABLE_CLEAR

    PUSH    AX
    MOVW    AX,ES:[HL]      ;loww(src)
    MOVW    HL,AX
    POP     AX
    MOV     ES,A
    BR      $.L3_RAM_INIT_TABLE_COPY

.L2_RAM_INIT_TABLE_COPY:
    MOV     A,ES:[HL]
    INCW    HL
    MOV     [DE],A
    INCW    DE

.L3_RAM_INIT_TABLE_COPY:
    MOVW    AX,DE
    CMPW    AX,BC
    BC      $.L2_RAM_INIT_TABLE_COPY
    BR      $.L4_RAM_INIT_TABLE

.L2_RAM_INIT_TABLE_CLEAR:
    MOV     [DE],#0
    INCW    DE

.L3_RAM_INIT_TABLE_CLEAR:
    MOVW    AX,DE
    CMPW    AX,BC
    BC      $.L2_RAM_INIT_TABLE_CLEAR

.L4_RAM_INIT_TABLE:
    POP     AX              ;table ponter
    ADDW    AX,#8

.L5_RAM_INIT_TABLE:
    CMPW    AX,#LOWW(STARTOF(.ram_init_table)+SIZEOF(.ram_init_table))
    BC      $.L1_RAM_INIT_TABLE

    CALL    !!_main
    CLRW    AX

_exit:
    BR      $_exit