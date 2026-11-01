import 'package:flutter_riverpod/flutter_riverpod.dart';
import '../models/memory_extraction_status.dart';
import 'connection_controller.dart';

final memoryExtractionProvider = FutureProvider<MemoryExtractionStatus>(
  (ref) => ref.watch(apiProvider).memoryExtractionStatus(),
);
