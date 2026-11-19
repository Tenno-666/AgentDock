import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_markdown_plus/flutter_markdown_plus.dart';

import '../app_state.dart';
import '../daemon_client.dart';
import '../widgets/approval_card.dart';

const _mono = 'monospace';

class ChatPage extends StatelessWidget {
  const ChatPage({super.key, required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([state, state.currentChat, state.navIndex]),
      builder: (context, _) {
        final ids = state.chats.keys.toList();
        if (ids.isEmpty) {
          return Scaffold(
            appBar: AppBar(title: const Text('对话')),
            body: const Center(child: Text('还没有接管的会话。\n在"会话"页点"接管"或"新建会话"。', textAlign: TextAlign.center)),
          );
        }
        var current = state.currentChat.value;
        if (current == null || !state.chats.containsKey(current)) current = ids.first;
        final idx = ids.indexOf(current);
        final visible = state.navIndex.value == 1;
        return DefaultTabController(
          key: ValueKey('tabs-${ids.join(',')}'),
          length: ids.length,
          initialIndex: idx,
          child: Builder(builder: (context) {
            final ctl = DefaultTabController.of(context);
            ctl.addListener(() {
              if (!ctl.indexIsChanging) state.currentChat.value = ids[ctl.index];
            });
            return Scaffold(
              appBar: AppBar(
                title: const Text('对话'),
                bottom: TabBar(
                  isScrollable: true,
                  tabs: [
                    for (final id in ids)
                      Tab(
                        child: Row(mainAxisSize: MainAxisSize.min, children: [
                          Text(id.substring(0, 8)),
                          if (state.approvals.values.any((a) => a['sessionId'] == id))
                            const Padding(padding: EdgeInsets.only(left: 4), child: Icon(Icons.circle, size: 8, color: Colors.red))
                          else if (state.chats[id]!.promptRunning)
                            const Padding(padding: EdgeInsets.only(left: 6), child: SizedBox(width: 10, height: 10, child: CircularProgressIndicator(strokeWidth: 2))),
                        ]),
                      ),
                  ],
                ),
              ),
              body: TabBarView(
                children: [
                  for (final id in ids)
                    _ChatView(key: ValueKey('chat-$id'), state: state, chat: state.chats[id]!, active: visible && id == current),
                ],
              ),
            );
          }),
        );
      },
    );
  }
}

/// Terminal-style transcript, modelled on the Kiro CLI.
class _ChatView extends StatefulWidget {
  const _ChatView({super.key, required this.state, required this.chat, required this.active});
  final AppState state;
  final ChatSession chat;

  /// Visible to the user: only then approvals pop up as a bottom sheet.
  final bool active;

  @override
  State<_ChatView> createState() => _ChatViewState();
}

class _ChatViewState extends State<_ChatView> {
  final _input = TextEditingController();
  final _scroll = ScrollController();
  final _focus = FocusNode();
  final _expanded = <ChatItem>{};
  final _dismissed = <String>{}; // approvals whose sheet the user closed
  bool _showThoughts = true;
  bool _stick = true; // follow new output
  String? _sheetFor;
  bool _jumping = false;

  @override
  void initState() {
    super.initState();
    _scroll.addListener(() {
      if (!_scroll.hasClients || _jumping) return;
      final atBottom = _scroll.position.maxScrollExtent - _scroll.offset < 48;
      if (atBottom != _stick) setState(() => _stick = atBottom);
    });
  }

  @override
  void dispose() {
    _input.dispose();
    _scroll.dispose();
    _focus.dispose();
    super.dispose();
  }

  void _snack(String msg) {
    if (mounted) ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(msg)));
  }

  /// Lazy lists only learn their real extent after laying out the tail, so jump
  /// repeatedly until the end stops moving. User scrolling is not tracked meanwhile.
  void _toBottom([int tries = 0]) {
    if (!mounted || !_scroll.hasClients) return;
    final pos = _scroll.position;
    if (pos.maxScrollExtent - pos.pixels <= 1 || tries > 8) {
      _jumping = false;
      return;
    }
    _jumping = true;
    _scroll.jumpTo(pos.maxScrollExtent);
    WidgetsBinding.instance.addPostFrameCallback((_) => _toBottom(tries + 1));
    WidgetsBinding.instance.scheduleFrame();
  }

  Future<void> _send() async {
    final text = _input.text.trim();
    if (text.isEmpty) return;
    try {
      await widget.state.sendPrompt(widget.chat.sessionId, text);
      _input.clear();
      setState(() => _stick = true);
    } on DaemonError catch (e) {
      _snack(e.message);
    }
  }

  Future<void> _openSheet(Json request) async {
    final id = '${request['approvalId']}';
    _sheetFor = id;
    await showApprovalSheet(context, widget.state, request);
    _sheetFor = null;
    if (widget.state.approvals.containsKey(id)) _dismissed.add(id); // closed without answering
  }

  void _maybeAutoOpenSheet() {
    if (!widget.active || _sheetFor != null) return;
    final pending = widget.state.approvals.values
        .where((a) => a['sessionId'] == widget.chat.sessionId && !_dismissed.contains('${a['approvalId']}'))
        .toList();
    if (pending.isEmpty) return;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted && _sheetFor == null && widget.state.approvals.containsKey('${pending.first['approvalId']}')) {
        _openSheet(pending.first);
      }
    });
  }

  Future<void> _detach() async {
    final origin = widget.chat.originPane;
    var handback = origin != null;
    final small = Theme.of(context).textTheme.bodySmall;
    final messenger = ScaffoldMessenger.of(context);
    final ok = await showDialog<bool>(
      context: context,
      builder: (c) => StatefulBuilder(
        builder: (c, set) => AlertDialog(
          title: const Text('断开会话？'),
          content: Column(mainAxisSize: MainAxisSize.min, crossAxisAlignment: CrossAxisAlignment.start, children: [
            if (origin != null)
              CheckboxListTile(
                key: const ValueKey('handback'),
                contentPadding: EdgeInsets.zero,
                value: handback,
                onChanged: (v) => set(() => handback = v ?? false),
                title: Text('交还到原终端窗格 $origin'),
                subtitle: const Text('在该窗格中自动执行 kiro-cli --resume-id 续接'),
              ),
            SelectableText(
              '手动续接：kiro-cli --resume-id ${widget.chat.sessionId}\n'
              '如果终端提示 "Failed to resume session"，说明续接超时并已静默新开会话，请 /quit 后重试。',
              style: small,
            ),
          ]),
          actions: [
            TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('取消')),
            FilledButton(key: const ValueKey('confirm-detach'), onPressed: () => Navigator.pop(c, true), child: const Text('断开')),
          ],
        ),
      ),
    );
    if (ok != true) return;
    try {
      final r = await widget.state.detach(widget.chat.sessionId, handback: handback);
      if (r?['handedBack'] == true) {
        messenger.showSnackBar(SnackBar(content: Text('已交还到终端 ${r?['pane']}')));
      } else if (r?['handbackError'] != null) {
        messenger.showSnackBar(SnackBar(content: Text('已断开，但未交还：${r?['handbackError']}')));
      }
    } on DaemonError catch (e) {
      messenger.showSnackBar(SnackBar(content: Text(e.message)));
    }
  }

  /// Kiro-like status line.
  (String, bool) _status() {
    final chat = widget.chat;
    if (chat.closed) return ('会话已关闭', false);
    if (widget.state.approvals.values.any((a) => a['sessionId'] == chat.sessionId)) return ('等待审批', false);
    if (!chat.promptRunning) return ('空闲', false);
    final last = chat.items.isEmpty ? null : chat.items.last;
    if (last?.kind == ChatKind.tool && !const {'completed', 'failed'}.contains(last?.data?['status'])) {
      return ('执行工具中', true);
    }
    if (last?.kind == ChatKind.thought) return ('思考中', true);
    return ('回复中', true);
  }

  @override
  Widget build(BuildContext context) {
    final chat = widget.chat;
    final items = chat.items.where((i) => _showThoughts || i.kind != ChatKind.thought).toList();
    if (_stick) WidgetsBinding.instance.addPostFrameCallback((_) => _toBottom());
    _maybeAutoOpenSheet();
    final dark = ThemeData(colorSchemeSeed: Colors.indigo, brightness: Brightness.dark, useMaterial3: true);
    final (status, busy) = _status();
    return Column(children: [
      Material(
        elevation: 1,
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 4),
          child: Row(children: [
            Expanded(child: SelectableText('${chat.sessionId}  ·  ${chat.cwd}', maxLines: 1, style: Theme.of(context).textTheme.bodySmall)),
            FilterChip(label: const Text('思考过程'), selected: _showThoughts, onSelected: (v) => setState(() => _showThoughts = v)),
            const SizedBox(width: 8),
            if (!chat.closed) TextButton.icon(onPressed: _detach, icon: const Icon(Icons.link_off), label: const Text('断开')),
          ]),
        ),
      ),
      Expanded(
        child: Theme(
          data: dark,
          child: Material(
            color: const Color(0xFF16161D),
            child: Stack(children: [
              ListView.builder(
                key: const ValueKey('transcript'),
                controller: _scroll,
                padding: const EdgeInsets.fromLTRB(14, 10, 14, 24),
                itemCount: items.length,
                itemBuilder: (context, i) => _item(context, items[i], i == 0),
              ),
              if (!_stick)
                Positioned(
                  right: 16,
                  bottom: 16,
                  child: FloatingActionButton.small(
                    key: const ValueKey('to-bottom'),
                    heroTag: null,
                    tooltip: '回到底部',
                    onPressed: () {
                      setState(() => _stick = true);
                      _toBottom();
                    },
                    child: const Icon(Icons.arrow_downward),
                  ),
                ),
            ]),
          ),
        ),
      ),
      Container(
        width: double.infinity,
        color: Theme.of(context).colorScheme.surfaceContainerHighest,
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 4),
        child: Row(children: [
          if (busy)
            const Padding(padding: EdgeInsets.only(right: 8), child: SizedBox(width: 10, height: 10, child: CircularProgressIndicator(strokeWidth: 2))),
          Text(status, key: const ValueKey('status-line'), style: const TextStyle(fontFamily: _mono, fontSize: 12)),
        ]),
      ),
      if (!chat.closed) _composer(context, chat),
    ]);
  }

  Widget _composer(BuildContext context, ChatSession chat) {
    return SafeArea(
      top: false,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 6, 12, 12),
        child: Row(crossAxisAlignment: CrossAxisAlignment.end, children: [
          Expanded(
            child: CallbackShortcuts(
              bindings: {const SingleActivator(LogicalKeyboardKey.enter, control: true): _send},
              child: TextField(
                key: const ValueKey('composer'),
                controller: _input,
                focusNode: _focus,
                minLines: 1,
                maxLines: 8,
                keyboardType: TextInputType.multiline,
                style: const TextStyle(fontFamily: _mono),
                decoration: const InputDecoration(hintText: 'ask a question or describe a task（Ctrl+Enter 发送）', border: OutlineInputBorder()),
              ),
            ),
          ),
          const SizedBox(width: 8),
          if (chat.promptRunning)
            IconButton.filledTonal(
              tooltip: '取消本轮',
              onPressed: () => widget.state.cancelPrompt(chat.sessionId).catchError((Object e) => _snack('$e')),
              icon: const Icon(Icons.stop),
            )
          else
            IconButton.filled(key: const ValueKey('send'), tooltip: '发送', onPressed: _send, icon: const Icon(Icons.send)),
        ]),
      ),
    );
  }

  Widget _dotLine(Color dot, Widget child, {VoidCallback? onTap}) {
    return InkWell(
      onTap: onTap,
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 3),
        child: Row(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Padding(padding: const EdgeInsets.only(top: 2, right: 8), child: Text('●', style: TextStyle(color: dot, fontSize: 12))),
          Expanded(child: child),
        ]),
      ),
    );
  }

  Widget _item(BuildContext context, ChatItem it, bool first) {
    final scheme = Theme.of(context).colorScheme;
    final base = TextStyle(fontFamily: _mono, fontSize: 13.5, height: 1.45, color: scheme.onSurface);
    switch (it.kind) {
      case ChatKind.user:
        return Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          if (!first) Divider(color: scheme.outlineVariant, height: 22),
          SelectableText('> ${it.text}', style: base.copyWith(color: scheme.primary, fontWeight: FontWeight.bold)),
          const SizedBox(height: 6),
        ]);
      case ChatKind.assistant:
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 2),
          child: MarkdownBody(
            data: it.text,
            selectable: true,
            styleSheet: MarkdownStyleSheet.fromTheme(Theme.of(context)).copyWith(
              p: base,
              listBullet: base,
              code: base.copyWith(backgroundColor: scheme.surfaceContainerHighest, fontSize: 13),
              codeblockDecoration: BoxDecoration(color: scheme.surfaceContainerHighest, borderRadius: BorderRadius.circular(6)),
            ),
          ),
        );
      case ChatKind.thought:
        return _dotLine(
          scheme.outline,
          Text('Thought  ${it.text.trim()}', style: base.copyWith(color: scheme.outline, fontStyle: FontStyle.italic, fontSize: 12.5)),
        );
      case ChatKind.tool:
        final d = it.data ?? {};
        final status = '${d['status'] ?? ''}';
        final color = switch (status) {
          'completed' => Colors.greenAccent,
          'failed' => Colors.redAccent,
          _ => Colors.amberAccent,
        };
        final raw = d['rawInput'];
        final command = raw is Map ? raw['command'] : null;
        final headline = command != null ? 'Shell $command' : '${d['title'] ?? '工具调用'}';
        final output = '${d['output'] ?? ''}'.trimRight();
        final lines = output.isEmpty ? <String>[] : output.split('\n');
        final open = _expanded.contains(it);
        const preview = 12;
        final shown = open || lines.length <= preview ? lines : lines.sublist(lines.length - preview);
        return _dotLine(
          color,
          Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
            Text(headline, style: base.copyWith(fontWeight: FontWeight.w600)),
            if (open && command == null && raw != null)
              SelectableText(prettyJson(raw), style: base.copyWith(color: scheme.outline, fontSize: 12)),
            if (lines.length > preview && !open)
              Text('  … 还有 ${lines.length - preview} 行（点击展开）', style: base.copyWith(color: scheme.outline, fontSize: 12)),
            if (shown.isNotEmpty)
              SelectableText('  ${shown.join('\n  ')}', style: base.copyWith(color: scheme.onSurfaceVariant, fontSize: 12.5)),
          ]),
          onTap: () => setState(() => open ? _expanded.remove(it) : _expanded.add(it)),
        );
      case ChatKind.approval:
        final req = it.data!;
        final res = it.resolution;
        final text = res == null
            ? '审批 ${req['approvalId']}：${req['title'] ?? ''}（等待处理，点击打开）'
            : '审批：已${optionLabel(res)}';
        return _dotLine(
          res == null ? Colors.orangeAccent : (res.startsWith('allow') ? Colors.greenAccent : Colors.redAccent),
          Text(text, key: ValueKey('approval-line-${req['approvalId']}'), style: base.copyWith(color: res == null ? Colors.orangeAccent : null)),
          onTap: res == null && widget.state.approvals.containsKey('${req['approvalId']}') ? () => _openSheet(req) : null,
        );
      case ChatKind.notice:
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: Center(child: Text('— ${it.text} —', style: base.copyWith(color: scheme.outline, fontSize: 12))),
        );
    }
  }
}
