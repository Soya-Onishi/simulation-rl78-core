# Renesas CC-RL build for the water-heater guest (RL78/G23).
CCRL ?= ccrl
CCRL_LIB ?= /usr/local/Renesas/CC-RL/V1.16.00/lib/rl78em4r.lib

.PHONY: build clean

build:
	$(CCRL) -cpu=S3 -Olite -g -dev=DR7F100GPL.DVF \
		-lnkopt=-DEBUG -lnkopt=-AUTO_SECTION_LAYOUT \
		main.c cstart.asm \
		-lnkopt=-LIBRARY=$(CCRL_LIB)

clean:
	rm -f main.abs *.obj *.rel *.lst *.map
