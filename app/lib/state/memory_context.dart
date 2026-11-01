import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../models/memory_context.dart';
import 'connection_controller.dart';

final memoryContextsProvider =
    FutureProvider.family<Map<String, MemoryContext>, String>((ref, ids) async {
      if (ids.isEmpty) return {};
      final rows = await ref.watch(apiProvider).memoryContexts(ids.split(','));
      return {for (final row in rows) row.factId: row};
    });
