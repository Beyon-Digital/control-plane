"""Async XDG RemoteDesktop + ScreenCast transport (dbus-next, session bus only)."""
import asyncio
import os
import uuid

DEST = "org.freedesktop.portal.Desktop"
PATH = "/org/freedesktop/portal/desktop"
REMOTE = "org.freedesktop.portal.RemoteDesktop"
CAST = "org.freedesktop.portal.ScreenCast"


def unpack(value):
    if hasattr(value, "value"):
        return unpack(value.value)
    if isinstance(value, dict):
        return {key: unpack(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [unpack(item) for item in value]
    return value


class DBusPortal:
    def __init__(self):
        self.bus = None
        self.requests = {}
        self.on_invalidated = None
        self.sessions = set()

    async def connect(self):
        from dbus_next.aio import MessageBus
        from dbus_next import Message, MessageType, Variant
        self.Message, self.Type, self.Variant = Message, MessageType, Variant
        self.bus = await MessageBus(negotiate_unix_fd=True).connect()
        self.bus.add_message_handler(self.handle_signal)
        # Subscribe before creating requests; portal responses can precede the
        # method return. Session and owner signals immediately invalidate access.
        for rule in ["type='signal',interface='org.freedesktop.portal.Request'",
                     "type='signal',interface='org.freedesktop.portal.Session'",
                     "type='signal',interface='org.gnome.ScreenSaver'",
                     "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.freedesktop.portal.Desktop'"]:
            await self.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "AddMatch", "s", [rule])

    async def call(self, destination, path, interface, member, signature="", body=None):
        reply = await asyncio.wait_for(self.bus.call(self.Message(destination=destination, path=path,
                    interface=interface, member=member, signature=signature, body=body or [])), 8)
        if reply.message_type == self.Type.ERROR:
            raise RuntimeError(f"{interface}.{member}: {reply.error_name}: {' '.join(map(str, reply.body))}")
        return reply

    def handle_signal(self, message):
        if message.message_type != self.Type.SIGNAL:
            return
        if message.interface == "org.freedesktop.portal.Request" and message.member == "Response":
            future = self.requests.get(message.path)
            if future and not future.done():
                future.set_result(message.body)
        elif (message.interface == "org.freedesktop.portal.Session" and message.member == "Closed" and message.path in self.sessions
              or message.interface == "org.gnome.ScreenSaver" and message.member == "ActiveChanged" and message.body == [True]
              or message.interface == "org.freedesktop.DBus" and message.member == "NameOwnerChanged" and message.body[0] == DEST and message.body[1]):
            if self.on_invalidated:
                asyncio.create_task(self.on_invalidated("Portal closed, permission revoked, or desktop locked"))

    async def request(self, interface, member, signature, body, options):
        token = "cp" + uuid.uuid4().hex
        options = dict(options, handle_token=self.Variant("s", token))
        sender = self.bus.unique_name[1:].replace(".", "_")
        expected = PATH + "/request/" + sender + "/" + token
        future = asyncio.get_running_loop().create_future()
        self.requests[expected] = future
        actual = expected
        completed = False
        try:
            response = await self.call(DEST, PATH, interface, member, signature, body + [options])
            actual = response.body[0]
            self.requests[actual] = future
            code, result = await asyncio.wait_for(future, 90)
            if code != 0:
                raise RuntimeError("Local portal authorization cancelled" if code == 1 else "Portal request denied")
            completed = True
            return unpack(result)
        finally:
            self.requests.pop(expected, None)
            self.requests.pop(actual, None)
            if not completed:
                try:
                    await self.call(DEST, actual, "org.freedesktop.portal.Request", "Close")
                except Exception:
                    pass

    def variants(self, options):
        return {key: self.Variant("b" if type(value) is bool else "u" if type(value) is int else "s", value)
                for key, value in options.items()}

    async def properties(self, interface):
        response = await self.call(DEST, PATH, "org.freedesktop.DBus.Properties", "GetAll", "s", [interface])
        return unpack(response.body[0])

    async def capabilities(self):
        remote, cast = await self.properties(REMOTE), await self.properties(CAST)
        return {"version": remote.get("version", 1), "devices": remote.get("AvailableDeviceTypes", 0),
                "cursors": cast.get("AvailableCursorModes", 1)}

    async def create(self):
        result = await self.request(REMOTE, "CreateSession", "a{sv}", [],
                                    {"session_handle_token": self.Variant("s", "cp" + uuid.uuid4().hex)})
        handle = result["session_handle"]
        self.sessions.add(handle)
        return handle

    async def select_devices(self, session, options):
        await self.request(REMOTE, "SelectDevices", "oa{sv}", [session], self.variants(options))

    async def select_sources(self, session, options):
        await self.request(CAST, "SelectSources", "oa{sv}", [session], self.variants(options))

    async def start(self, session):
        return await self.request(REMOTE, "Start", "osa{sv}", [session, ""], {})

    async def open_remote(self, session):
        response = await self.call(DEST, PATH, CAST, "OpenPipeWireRemote", "oa{sv}", [session, {}])
        index = response.body[0]
        if not response.unix_fds or index >= len(response.unix_fds):
            raise RuntimeError("Portal did not pass a PipeWire descriptor")
        result = os.dup(response.unix_fds[index])
        for fd in response.unix_fds:
            os.close(fd)
        return result

    async def unlocked(self):
        try:
            result = await self.call("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver", "org.gnome.ScreenSaver", "GetActive")
            return result.body == [False]
        except Exception:
            return False

    async def close(self, session):
        self.sessions.discard(session)
        try:
            await self.call(DEST, session, "org.freedesktop.portal.Session", "Close")
        except Exception:
            pass

    async def notify(self, session, method, *values):
        signatures = {"NotifyPointerMotionAbsolute": "udd", "NotifyPointerButton": "iu",
                      "NotifyPointerAxis": "dd", "NotifyKeyboardKeysym": "iu"}
        options = {"finish": self.Variant("b", True)} if method == "NotifyPointerAxis" else {}
        await self.call(DEST, PATH, REMOTE, method, "oa{sv}" + signatures[method], [session, options] + list(values))
