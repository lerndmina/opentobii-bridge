/*
 * sesp_relay: owns \\.\pipe\streamengineservices inside a Wine prefix and
 * relays it to a service on the Linux side over localhost TCP.
 *
 * The Tobii game integration DLL opens that pipe by name for head pose and
 * for the display id it matches against a monitor. A Linux process cannot own
 * a Wine named pipe, so this program does and forwards the bytes. It knows
 * nothing about the protocol beyond the pipe handshake.
 *
 * Per client channel:
 *   1. The client writes a hello: u32 channel, its reply pipe name, NUL.
 *   2. We open that reply pipe and write one u32, our slot number.
 *   3. We connect to 127.0.0.1:RELAY_PORT and send our own hello (below).
 *   4. Bytes from the client go to TCP; bytes from TCP go to the reply pipe.
 *
 * Hello to the Linux service, little-endian:
 *   "OTB1", u32 channel, u32 client pid, u32 slot,
 *   u64 QueryPerformanceCounter in microseconds,
 *   u32 monitor count, then per monitor: u8 primary, u16 length, UTF-8
 *   device instance id.
 *
 * Build: x86_64-w64-mingw32-gcc -O2 -Wall -o sesp_relay.exe sesp_relay.c \
 *            -lws2_32 -lcfgmgr32 -ladvapi32 -static
 *
 * Runs as a Windows service when the service manager starts it, and in the
 * foreground otherwise.
 */

#define WIN32_LEAN_AND_MEAN
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <initguid.h>
#include <devpropdef.h>
#include <devpkey.h>
#include <cfgmgr32.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define PIPE_NAME "\\\\.\\pipe\\streamengineservices"
#define SERVICE_NAME "OpenTobiiRelay"
#define RELAY_PORT 47455
#define BUF 65536
#define MAX_MONITORS 16

static volatile LONG next_slot = 0;

static void logf_(const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    vfprintf(stderr, fmt, args);
    va_end(args);
    fputc('\n', stderr);
    fflush(stderr);
}

static uint64_t qpc_micros(void) {
    LARGE_INTEGER count, freq;
    QueryPerformanceCounter(&count);
    QueryPerformanceFrequency(&freq);
    return (uint64_t)(count.QuadPart / freq.QuadPart) * 1000000ull +
           (uint64_t)(count.QuadPart % freq.QuadPart) * 1000000ull / (uint64_t)freq.QuadPart;
}

static int send_all(SOCKET s, const void *data, int len) {
    const char *p = data;
    while (len > 0) {
        int n = send(s, p, len, 0);
        if (n <= 0) return 0;
        p += n;
        len -= n;
    }
    return 1;
}

/* Append the active monitors' device instance ids, found the way the game
 * DLL finds them: the monitor's device interface name, then its
 * DEVPKEY_Device_InstanceId. Returns the number written. */
static uint32_t append_monitors(uint8_t *out, size_t cap, size_t *used) {
    uint32_t count = 0;
    DISPLAY_DEVICEW adapter = {.cb = sizeof adapter};
    for (DWORD a = 0; EnumDisplayDevicesW(NULL, a, &adapter, 0); a++) {
        if (!(adapter.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)) continue;
        DISPLAY_DEVICEW mon = {.cb = sizeof mon};
        for (DWORD m = 0;
             EnumDisplayDevicesW(adapter.DeviceName, m, &mon, EDD_GET_DEVICE_INTERFACE_NAME);
             m++) {
            WCHAR instance[512];
            ULONG size = sizeof instance;
            DEVPROPTYPE type;
            if (!(mon.StateFlags & DISPLAY_DEVICE_ACTIVE)) continue;
            if (CM_Get_Device_Interface_PropertyW(mon.DeviceID, &DEVPKEY_Device_InstanceId,
                                                  &type, (PBYTE)instance, &size, 0) != CR_SUCCESS)
                continue;
            char utf8[1024];
            int len = WideCharToMultiByte(CP_UTF8, 0, instance, -1, utf8, sizeof utf8, NULL, NULL);
            if (len <= 1 || count == MAX_MONITORS || *used + 3 + (size_t)len > cap) continue;
            len -= 1; /* drop the NUL */
            out[(*used)++] = (adapter.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE) ? 1 : 0;
            out[(*used)++] = (uint8_t)(len & 0xff);
            out[(*used)++] = (uint8_t)(len >> 8);
            memcpy(out + *used, utf8, (size_t)len);
            *used += (size_t)len;
            count++;
        }
    }
    return count;
}

struct link {
    HANDLE pipe;  /* the client writes requests here */
    HANDLE reply; /* we write responses here */
    SOCKET sock;
    LONG slot;
};

static DWORD WINAPI tcp_to_reply(LPVOID arg) {
    struct link *l = arg;
    static __thread char buf[BUF];
    for (;;) {
        int n = recv(l->sock, buf, sizeof buf, 0);
        if (n <= 0) break;
        DWORD done = 0, written;
        while (done < (DWORD)n) {
            if (!WriteFile(l->reply, buf + done, (DWORD)n - done, &written, NULL)) goto out;
            done += written;
        }
    }
out:
    /* Wake the pipe reader so the connection is torn down from one place. */
    CancelIoEx(l->pipe, NULL);
    DisconnectNamedPipe(l->pipe);
    return 0;
}

static DWORD WINAPI serve(LPVOID arg) {
    struct link *l = arg;
    static __thread char buf[BUF];
    DWORD got;
    HANDLE pump = NULL;
    l->reply = INVALID_HANDLE_VALUE;
    l->sock = INVALID_SOCKET;

    if (!ReadFile(l->pipe, buf, sizeof buf - 1, &got, NULL) || got < 6) {
        logf_("slot %ld: no hello (error %lu)", l->slot, GetLastError());
        goto done;
    }
    buf[got] = 0;
    uint32_t channel;
    memcpy(&channel, buf, 4);
    char reply_path[512];
    snprintf(reply_path, sizeof reply_path, "\\\\.\\pipe\\%s", buf + 4);

    ULONG client_pid = 0;
    GetNamedPipeClientProcessId(l->pipe, &client_pid);

    l->reply = CreateFileA(reply_path, GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
    if (l->reply == INVALID_HANDLE_VALUE) {
        logf_("slot %ld: cannot open %s (error %lu)", l->slot, reply_path, GetLastError());
        goto done;
    }

    l->sock = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_port = htons(RELAY_PORT)};
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (connect(l->sock, (struct sockaddr *)&addr, sizeof addr) != 0) {
        logf_("slot %ld: no service on 127.0.0.1:%d", l->slot, RELAY_PORT);
        goto done;
    }
    int nodelay = 1;
    setsockopt(l->sock, IPPROTO_TCP, TCP_NODELAY, (const char *)&nodelay, sizeof nodelay);

    uint8_t hello[4096];
    size_t used = 0;
    uint32_t slot = (uint32_t)l->slot, pid = client_pid;
    uint64_t now = qpc_micros();
    memcpy(hello + used, "OTB1", 4); used += 4;
    memcpy(hello + used, &channel, 4); used += 4;
    memcpy(hello + used, &pid, 4); used += 4;
    memcpy(hello + used, &slot, 4); used += 4;
    memcpy(hello + used, &now, 8); used += 8;
    size_t count_at = used;
    used += 4;
    uint32_t monitors = append_monitors(hello, sizeof hello, &used);
    memcpy(hello + count_at, &monitors, 4);
    if (!send_all(l->sock, hello, (int)used)) goto done;

    /* Only now tell the client it is connected: the service is listening. */
    DWORD written;
    if (!WriteFile(l->reply, &slot, 4, &written, NULL)) goto done;
    logf_("slot %ld: channel %#x from pid %lu, %u monitors", l->slot, channel, client_pid, monitors);

    pump = CreateThread(NULL, 0, tcp_to_reply, l, 0, NULL);
    while (ReadFile(l->pipe, buf, sizeof buf, &got, NULL) || GetLastError() == ERROR_MORE_DATA) {
        if (got && !send_all(l->sock, buf, (int)got)) break;
    }

done:
    if (l->sock != INVALID_SOCKET) {
        shutdown(l->sock, SD_BOTH);
        closesocket(l->sock);
    }
    if (pump) {
        WaitForSingleObject(pump, 2000);
        CloseHandle(pump);
    }
    if (l->reply != INVALID_HANDLE_VALUE) CloseHandle(l->reply);
    DisconnectNamedPipe(l->pipe);
    CloseHandle(l->pipe);
    logf_("slot %ld: closed", l->slot);
    HeapFree(GetProcessHeap(), 0, l);
    return 0;
}

static int relay_loop(void) {
    WSADATA wsa;
    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return 1;
    logf_("sesp_relay: serving %s, relaying to 127.0.0.1:%d", PIPE_NAME, RELAY_PORT);

    for (;;) {
        HANDLE pipe = CreateNamedPipeA(
            PIPE_NAME, PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES, BUF, BUF, 0, NULL);
        if (pipe == INVALID_HANDLE_VALUE) {
            logf_("CreateNamedPipe failed (error %lu)", GetLastError());
            return 1;
        }
        if (!ConnectNamedPipe(pipe, NULL) && GetLastError() != ERROR_PIPE_CONNECTED) {
            CloseHandle(pipe);
            continue;
        }
        struct link *l = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof *l);
        l->pipe = pipe;
        l->slot = InterlockedIncrement(&next_slot);
        HANDLE thread = CreateThread(NULL, 0, serve, l, 0, NULL);
        if (thread) CloseHandle(thread);
    }
}

/* Service plumbing. Wine's services.exe starts us when the prefix boots and
 * ends us when the last ordinary process exits, so the relay never keeps a
 * game's prefix alive. */

static SERVICE_STATUS_HANDLE status_handle;

static void report(DWORD state) {
    SERVICE_STATUS status = {
        .dwServiceType = SERVICE_WIN32_OWN_PROCESS,
        .dwCurrentState = state,
        .dwControlsAccepted = state == SERVICE_RUNNING ? SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN : 0,
    };
    SetServiceStatus(status_handle, &status);
}

static DWORD WINAPI on_control(DWORD control, DWORD type, LPVOID data, LPVOID context) {
    (void)type; (void)data; (void)context;
    if (control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN) {
        report(SERVICE_STOPPED);
        ExitProcess(0);
    }
    return control == SERVICE_CONTROL_INTERROGATE ? NO_ERROR : ERROR_CALL_NOT_IMPLEMENTED;
}

static void WINAPI service_main(DWORD argc, LPSTR *argv) {
    (void)argc; (void)argv;
    status_handle = RegisterServiceCtrlHandlerExA(SERVICE_NAME, on_control, NULL);
    if (!status_handle) return;
    report(SERVICE_RUNNING);
    relay_loop();
    report(SERVICE_STOPPED);
}

int main(void) {
    SERVICE_TABLE_ENTRYA table[] = {{(LPSTR)SERVICE_NAME, service_main}, {NULL, NULL}};
    if (StartServiceCtrlDispatcherA(table)) return 0;
    if (GetLastError() != ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) return 1;
    /* Not started by the service manager: run in the foreground. */
    return relay_loop();
}
