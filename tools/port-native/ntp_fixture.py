"""Serve the host's actual UTC to an isolated QEMU guest over SNTP."""
import socket
import struct
import threading
import time

class Clock:
    def __init__(self):
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.socket.bind(('127.0.0.1', 0))
        self.socket.settimeout(.25)
        self.port = self.socket.getsockname()[1]
        self.stopped = threading.Event()
        self.answers = 0
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    @staticmethod
    def stamp(now):
        now += 2208988800
        return struct.pack('!II', int(now), int((now % 1) * (1 << 32)))

    def run(self):
        while not self.stopped.is_set():
            try:
                request, address = self.socket.recvfrom(512)
            except socket.timeout:
                continue
            if len(request) < 48:
                continue
            now = self.stamp(time.time())
            response = bytes([0x24, 2, 6, 0xEC]) + bytes(8) + b'LOCL' + now + request[40:48] + now + self.stamp(time.time())
            self.socket.sendto(response, address)
            self.answers += 1

    def close(self):
        self.stopped.set()
        self.thread.join(timeout=1)
        self.socket.close()
