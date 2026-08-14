import 'package:http/http.dart' as http;

/// 原生（Windows / macOS / Linux）实现。
http.Client createHttpClient() => http.Client();

const String kHttpClientKind = 'dart:io (IOClient)';
