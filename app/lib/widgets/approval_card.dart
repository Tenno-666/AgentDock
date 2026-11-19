import 'package:flutter/material.dart';

import '../app_state.dart';
import '../daemon_client.dart';

const _optionLabels = {
  'allow_once': '允许一次',
  'allow_always': '总是允许',
  'reject_once': '拒绝',
  'cancelled': '取消',
};

String optionLabel(String id) => _optionLabels[id] ?? id;

/// Shows exactly what will run (rawInput) and the agent-offered options.
class ApprovalCard extends StatefulWidget {
  const ApprovalCard({
    super.key,
    required this.state,
    required this.request,
    this.resolution,
    this.showSession = false,
    this.sheet = false,
  });

  final AppState state;
  final Json request;
  final String? resolution;
  final bool showSession;

  /// Phone-friendly layout for the bottom sheet: big full-width buttons,
  /// scrollable parameters.
  final bool sheet;

  @override
  State<ApprovalCard> createState() => _ApprovalCardState();
}

class _ApprovalCardState extends State<ApprovalCard> {
  bool _busy = false;

  Future<void> _reply(String optionId) async {
    if (optionId == 'allow_always') {
      final ok = await showDialog<bool>(
        context: context,
        builder: (c) => AlertDialog(
          title: const Text('确认"总是允许"？'),
          content: const Text('Kiro 可能会把这条信任规则保存下来，之后同类调用不再询问，也会影响终端里的会话。'),
          actions: [
            TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('取消')),
            FilledButton(onPressed: () => Navigator.pop(c, true), child: const Text('总是允许')),
          ],
        ),
      );
      if (ok != true) return;
    }
    setState(() => _busy = true);
    try {
      await widget.state.replyApproval('${widget.request['approvalId']}', optionId);
    } on DaemonError catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(e.message)));
      }
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final r = widget.request;
    final scheme = Theme.of(context).colorScheme;
    final pending = widget.resolution == null;
    final unknown = r['inputUnknown'] == true;
    final options = List<Map<String, dynamic>>.from(r['options'] ?? const []);
    return Card(
      key: ValueKey('approval-${r['approvalId']}${widget.sheet ? '-sheet' : ''}'),
      color: pending ? scheme.tertiaryContainer : scheme.surfaceContainerHighest,
      margin: const EdgeInsets.symmetric(vertical: 6),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(children: [
              Icon(pending ? Icons.gpp_maybe : Icons.verified_user, size: 20),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  '审批请求 ${r['approvalId']} · ${r['toolName'] ?? '?'}（${r['kind'] ?? '?'}）',
                  style: const TextStyle(fontWeight: FontWeight.bold),
                ),
              ),
            ]),
            if (widget.showSession)
              Text('会话 ${r['sessionId']}', style: Theme.of(context).textTheme.bodySmall),
            const SizedBox(height: 6),
            Text('${r['title'] ?? ''}'),
            const SizedBox(height: 6),
            if (unknown)
              Text('⚠ 未能获取完整参数，请谨慎批准', style: TextStyle(color: scheme.error, fontWeight: FontWeight.bold))
            else
              ConstrainedBox(
                constraints: BoxConstraints(maxHeight: widget.sheet ? 320 : double.infinity),
                child: Container(
                  width: double.infinity,
                  padding: const EdgeInsets.all(8),
                  decoration: BoxDecoration(
                    color: scheme.surface,
                    borderRadius: BorderRadius.circular(6),
                  ),
                  child: SingleChildScrollView(
                    child: SelectableText(
                      prettyJson(r['rawInput']),
                      style: const TextStyle(fontFamily: 'monospace', fontSize: 13),
                    ),
                  ),
                ),
              ),
            const SizedBox(height: 8),
            if (pending && widget.sheet)
              Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  for (final o in options)
                    Padding(
                      padding: const EdgeInsets.only(top: 8),
                      child: SizedBox(height: 52, child: _button('${o['optionId']}', '${o['kind'] ?? ''}')),
                    ),
                ],
              )
            else if (pending)
              Wrap(
                spacing: 8,
                children: [
                  for (final o in options)
                    _button('${o['optionId']}', '${o['kind'] ?? ''}'),
                ],
              )
            else
              Text('已处理：${optionLabel(widget.resolution!)}',
                  style: const TextStyle(fontWeight: FontWeight.bold)),
          ],
        ),
      ),
    );
  }

  Widget _button(String optionId, String kind) {
    final label = Text(optionLabel(optionId));
    final onPressed = _busy ? null : () => _reply(optionId);
    final key = ValueKey('opt-$optionId-${widget.request['approvalId']}');
    if (kind.startsWith('reject')) {
      return OutlinedButton(key: key, onPressed: onPressed, child: label);
    }
    if (kind == 'allow_once') {
      return FilledButton(key: key, onPressed: onPressed, child: label);
    }
    return TextButton(key: key, onPressed: onPressed, child: label);
  }
}

/// Bottom sheet for answering one approval; closes itself once it is resolved
/// (here or on another device).
class ApprovalSheet extends StatefulWidget {
  const ApprovalSheet({super.key, required this.state, required this.request});
  final AppState state;
  final Json request;

  @override
  State<ApprovalSheet> createState() => _ApprovalSheetState();
}

class _ApprovalSheetState extends State<ApprovalSheet> {
  bool _closing = false;

  @override
  void initState() {
    super.initState();
    widget.state.addListener(_check);
  }

  @override
  void dispose() {
    widget.state.removeListener(_check);
    super.dispose();
  }

  void _check() {
    if (_closing || widget.state.approvals.containsKey('${widget.request['approvalId']}')) return;
    _closing = true;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted && ModalRoute.of(context)?.isCurrent == true) Navigator.of(context).pop();
    });
  }

  @override
  Widget build(BuildContext context) {
    return SafeArea(
      child: SingleChildScrollView(
        padding: const EdgeInsets.fromLTRB(12, 0, 12, 12),
        child: ApprovalCard(state: widget.state, request: widget.request, sheet: true),
      ),
    );
  }
}

Future<void> showApprovalSheet(BuildContext context, AppState state, Json request) {
  return showModalBottomSheet<void>(
    context: context,
    isScrollControlled: true,
    showDragHandle: true,
    builder: (_) => ApprovalSheet(state: state, request: request),
  );
}
