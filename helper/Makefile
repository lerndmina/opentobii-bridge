CC = x86_64-w64-mingw32-gcc
CFLAGS = -O2 -Wall -Wextra

sesp_relay.exe: sesp_relay.c
	$(CC) $(CFLAGS) -o $@ $< -lws2_32 -lcfgmgr32 -ladvapi32 -static

clean:
	rm -f sesp_relay.exe

.PHONY: clean
