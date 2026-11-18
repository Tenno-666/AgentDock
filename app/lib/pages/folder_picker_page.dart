import 'package:flutter/material.dart';

import '../app_state.dart';
import '../daemon_client.dart';

/// Browse folders on the computer running the daemon and start a session in one.
/// Returns nothing; on success the new session is opened by [AppState.newSession].
class FolderPickerPage extends StatefulWidget {
  const FolderPickerPage({super.key, required this.state, this.initialPath});
  final AppState state;
  final String? initialPath;

  @override
  State<FolderPickerPage> createState() => _FolderPickerPageState();
}

class _FolderPickerPageState extends State<FolderPickerPage> {
  Json? _listing;
  String? _error;
  bool _loading = false;
  bool _showHidden = false;
  bool _creating = false;

  @override
  void initState() {
    super.initState();
    _load(widget.initialPath);
  }

  void _snack(String m) {
    if (mounted) ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(m)));
  }

  Future<void> _load(String? path) async {
    setState(() => _loading = true);
    try {
      final l = await widget.state.fsList(path, showHidden: _showHidden);
      setState(() {
        _listing = l;
        _error = null;
      });
    } on DaemonError catch (e) {
      // initial path may be gone: fall back to HOME once
      if (_listing == null && path != null) return _load(null);
      setState(() => _error = e.message);
    } finally {
      if (mounted) setState(() => _loading = false);
    }
  }

  String get _path => '${_listing?['path'] ?? ''}';

  Future<String?> _askName(String title, String initial) {
    final ctl = TextEditingController(text: initial);
    return showDialog<String>(
      context: context,
      builder: (c) => AlertDialog(
        title: Text(title),
        content: TextField(
          key: const ValueKey('folder-name'),
          controller: ctl,
          autofocus: true,
          decoration: const InputDecoration(labelText: '文件夹名'),
          onSubmitted: (v) => Navigator.pop(c, v.trim()),
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(c), child: const Text('取消')),
          FilledButton(key: const ValueKey('folder-name-ok'), onPressed: () => Navigator.pop(c, ctl.text.trim()), child: const Text('确定')),
        ],
      ),
    );
  }

  Future<void> _mkdir() async {
    final name = await _askName('新建文件夹', '');
    if (name == null || name.isEmpty) return;
    try {
      await widget.state.fsMkdir(_path, name);
      await _load(_path);
    } on DaemonError catch (e) {
      _snack(e.message);
    }
  }

  Future<void> _rename(Json e) async {
    final name = await _askName('重命名', '${e['name']}');
    if (name == null || name.isEmpty || name == e['name']) return;
    try {
      await widget.state.fsRename('${e['path']}', name);
    } on DaemonError catch (err) {
      if (err.code != 'CONFIRM_REQUIRED') {
        _snack(err.message);
        return;
      }
      if (!mounted) return;
      final ok = await showDialog<bool>(
        context: context,
        builder: (c) => AlertDialog(
          title: const Text('确认重命名？'),
          content: Text(err.message),
          actions: [
            TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('取消')),
            FilledButton(onPressed: () => Navigator.pop(c, true), child: const Text('仍要重命名')),
          ],
        ),
      );
      if (ok != true) return;
      try {
        await widget.state.fsRename('${e['path']}', name, force: true);
      } on DaemonError catch (e2) {
        _snack(e2.message);
        return;
      }
    }
    await _load(_path);
  }

  Future<void> _createSession() async {
    setState(() => _creating = true);
    try {
      await widget.state.newSession(_path);
      if (mounted) Navigator.of(context).pop();
    } on DaemonError catch (e) {
      _snack(e.message);
    } finally {
      if (mounted) setState(() => _creating = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final l = _listing;
    final entries = List<Json>.from(l?['entries'] ?? const []);
    final root = '${l?['root'] ?? ''}';
    // breadcrumb segments below root
    final rel = _path.length > root.length ? _path.substring(root.length + 1).split('/') : <String>[];
    return Scaffold(
      appBar: AppBar(
        title: const Text('选择工作目录'),
        actions: [
          IconButton(
            tooltip: _showHidden ? '隐藏隐藏文件夹' : '显示隐藏文件夹',
            icon: Icon(_showHidden ? Icons.visibility : Icons.visibility_off),
            onPressed: () {
              setState(() => _showHidden = !_showHidden);
              _load(_path);
            },
          ),
          IconButton(key: const ValueKey('mkdir'), tooltip: '新建文件夹', icon: const Icon(Icons.create_new_folder_outlined), onPressed: l == null ? null : _mkdir),
        ],
      ),
      body: Column(children: [
        if (l != null)
          SizedBox(
            height: 44,
            child: ListView(
              scrollDirection: Axis.horizontal,
              padding: const EdgeInsets.symmetric(horizontal: 8),
              children: [
                ActionChip(avatar: const Icon(Icons.home, size: 16), label: const Text('~'), onPressed: () => _load(root)),
                for (var i = 0; i < rel.length; i++) ...[
                  const Icon(Icons.chevron_right, size: 18),
                  ActionChip(label: Text(rel[i]), onPressed: () => _load('$root/${rel.sublist(0, i + 1).join('/')}')),
                ],
              ],
            ),
          ),
        if (_loading) const LinearProgressIndicator(),
        if (_error != null) Padding(padding: const EdgeInsets.all(12), child: Text(_error!, style: TextStyle(color: Theme.of(context).colorScheme.error))),
        Expanded(
          child: ListView(children: [
            if (l?['parent'] != null)
              ListTile(leading: const Icon(Icons.arrow_upward), title: const Text('上一级'), onTap: () => _load('${l!['parent']}')),
            for (final e in entries)
              ListTile(
                key: ValueKey('dir-${e['name']}'),
                leading: Icon(e['hidden'] == true ? Icons.folder_off_outlined : Icons.folder_outlined),
                title: Text('${e['name']}'),
                subtitle: (e['sessionCount'] as num? ?? 0) > 0 ? Text('${e['sessionCount']} 个会话') : null,
                onTap: () => _load('${e['path']}'),
                trailing: IconButton(
                  key: ValueKey('rename-${e['name']}'),
                  tooltip: '重命名',
                  icon: const Icon(Icons.drive_file_rename_outline),
                  onPressed: () => _rename(e),
                ),
              ),
            if (l != null && entries.isEmpty) const ListTile(title: Text('（没有子文件夹）')),
          ]),
        ),
        SafeArea(
          top: false,
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Row(children: [
              Expanded(
                child: Text(
                  l == null ? '' : '$_path${(l['sessionsHere'] ?? 0) > 0 ? '  ·  此处已有 ${l['sessionsHere']} 个会话' : ''}',
                  maxLines: 2,
                  overflow: TextOverflow.ellipsis,
                ),
              ),
              FilledButton.icon(
                key: const ValueKey('create-here'),
                onPressed: l == null || _creating ? null : _createSession,
                icon: _creating
                    ? const SizedBox(width: 16, height: 16, child: CircularProgressIndicator(strokeWidth: 2))
                    : const Icon(Icons.add_comment_outlined),
                label: const Text('在此处新建会话'),
              ),
            ]),
          ),
        ),
      ]),
    );
  }
}
