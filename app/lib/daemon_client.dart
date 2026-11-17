import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

/// Error reply from the daemon: {"type":"error","code":...,"message":...}.
class DaemonError implements Exception {
  DaemonError(this.code, this.message);
  final String code;
  final String message;
  @override
  String toString() => '[$code] $message';
}

enum ConnState { connecting, connected, disconnected }

typedef ChannelFactory = WebSocketChannel Function(Uri uri);

/// One long-lived connection to agentdock-daemon.
///
/// Requests get an incrementing id that the daemon echoes; messages without a
/// matching id are events and go to [events]. Reconnects with backoff.
class DaemonClient {
  DaemonClient(this.uri, {ChannelFactory? factory})
    : _factory = factory ?? WebSocketChannel.connect;

  final Uri uri;
  final ChannelFactory _factory;
  final ValueNotifier<ConnState> state = ValueNotifier(ConnState.disconnected);
  final _events = StreamController<Map<String, dynamic>>.broadcast();
  final _pending = <int, Completer<Map<String, dynamic>>>{};
  WebSocketChannel? _channel;
  StreamSubscription<dynamic>? _sub;
  int _nextId = 1;
  bool _closed = false;
  Duration _backoff = const Duration(milliseconds: 500);
  Timer? _retry;

  /// Fires after every successful (re)connect, so callers can resync state.
  final _connected = StreamController<void>.broadcast();
  Stream<void> get onConnected => _connected.stream;
  Stream<Map<String, dynamic>> get events => _events.stream;

  Future<void> connect() async {
    if (_closed) return;
    _retry?.cancel();
    state.value = ConnState.connecting;
    try {
      final ch = _factory(uri);
      await ch.ready;
      _channel = ch;
      _sub = ch.stream.listen(
        _onMessage,
        onDone: _onDisconnect,
        onError: (_) => _onDisconnect(),
      );
      _backoff = const Duration(milliseconds: 500);
      state.value = ConnState.connected;
      _connected.add(null);
    } catch (_) {
      _onDisconnect();
    }
  }

  void _onMessage(dynamic data) {
    if (data is! String) return;
    final Map<String, dynamic> msg;
    try {
      msg = jsonDecode(data) as Map<String, dynamic>;
    } catch (_) {
      return;
    }
    final id = msg['id'];
    if (id is int && _pending.containsKey(id)) {
      final c = _pending.remove(id)!;
      if (msg['type'] == 'error') {
        c.completeError(
          DaemonError('${msg['code'] ?? '?'}', '${msg['message'] ?? ''}'),
        );
      } else {
        c.complete(msg);
      }
      return;
    }
    _events.add(msg);
  }

  void _onDisconnect() {
    _sub?.cancel();
    _sub = null;
    _channel = null;
    for (final c in _pending.values) {
      c.completeError(DaemonError('DISCONNECTED', '与 daemon 的连接已断开'));
    }
    _pending.clear();
    if (_closed) {
      state.value = ConnState.disconnected;
      return;
    }
    state.value = ConnState.disconnected;
    _retry = Timer(_backoff, connect);
    final next = _backoff * 2;
    _backoff = next > const Duration(seconds: 10)
        ? const Duration(seconds: 10)
        : next;
  }

  Future<Map<String, dynamic>> request(
    String type, [
    Map<String, dynamic> params = const {},
    Duration timeout = const Duration(seconds: 120),
  ]) {
    final ch = _channel;
    if (ch == null) {
      return Future.error(DaemonError('DISCONNECTED', '未连接到 daemon'));
    }
    final id = _nextId++;
    final c = Completer<Map<String, dynamic>>();
    _pending[id] = c;
    ch.sink.add(jsonEncode({...params, 'id': id, 'type': type}));
    return c.future.timeout(
      timeout,
      onTimeout: () {
        _pending.remove(id);
        throw DaemonError('TIMEOUT', '$type 请求超时');
      },
    );
  }

  Future<void> close() async {
    _closed = true;
    _retry?.cancel();
    await _channel?.sink.close();
    await _sub?.cancel();
    state.value = ConnState.disconnected;
  }
}
