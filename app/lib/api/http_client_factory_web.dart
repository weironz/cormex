import 'package:fetch_client/fetch_client.dart';
import 'package:http/http.dart' as http;

/// Web 实现，建在浏览器的 `fetch` 上。
///
/// 两个参数是要紧的：
///
/// * [RequestMode.cors] —— `fetch_client` 默认是 `no-cors`，那会拿到一个
///   **不透明**响应：状态码 0、body 读不出来。Cormex 的 Web 界面几乎总是
///   跟 cortexd 不同源（要么本地起在 8080，要么部署在另一个域），所以必须
///   显式要一个真正的 CORS 响应。cortexd 那边已经挂了 CORS 层。
/// * `credentials: omit` —— 凭据走 `Authorization` 头，不走 cookie。带上
///   cookie 只会让 CORS 变严（`Access-Control-Allow-Origin: *` 当场失效），
///   换来的是这个应用根本不用的东西。
http.Client createHttpClient() => FetchClient(
  mode: RequestMode.cors,
  credentials: RequestCredentials.omit,
);

const String kHttpClientKind = 'browser fetch (FetchClient)';
