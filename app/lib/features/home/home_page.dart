import 'package:flutter/material.dart';

import '../memory/memory_browser.dart';
import '../sessions/sessions_page.dart';

/// 连上之后的两个页面：记忆与会话档案。
///
/// # 为什么用底部导航而不是抽屉
///
/// 只有两个页面，而且它们是**并列**的两件事：「我的记忆里有什么」和
/// 「我的 agent 到底说了什么」。抽屉会把第二个藏起来，而它恰恰是回答
/// 「凭什么这么认为」的地方 —— 藏起来等于把出处放到了第二层。
class HomePage extends StatefulWidget {
  const HomePage({super.key});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  int _index = 0;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: IndexedStack(
        index: _index,
        // `IndexedStack` 而不是按需构建：切回来时滚动位置与检索结果还在，
        // 而重建会把它们都丢掉 —— 在记忆浏览器里那意味着刚调好的回放点
        // 没了
        children: const [MemoryBrowser(), SessionsPage()],
      ),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _index,
        onDestinationSelected: (i) => setState(() => _index = i),
        destinations: const [
          NavigationDestination(icon: Icon(Icons.search), label: '记忆'),
          NavigationDestination(
            icon: Icon(Icons.forum_outlined),
            selectedIcon: Icon(Icons.forum),
            label: '会话档案',
          ),
        ],
      ),
    );
  }
}
