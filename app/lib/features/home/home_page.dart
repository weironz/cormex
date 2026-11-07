import 'package:flutter/material.dart';

import '../../core/theme.dart';
import '../memory/memory_browser.dart';
import '../sessions/sessions_page.dart';

/// 连上之后的两个页面：记忆与聊天档案。
///
/// # 为什么两个页面并列，而不是一个藏进抽屉
///
/// 它们是**并列**的两件事：「我的记忆里有什么」和「我的 agent 到底说了什么」。
/// 抽屉会把第二个藏起来，而它恰恰是回答「凭什么这么认为」的地方 —— 藏起来等于
/// 把出处放到了第二层。
///
/// # 为什么宽屏用侧栏、窄屏用底栏
///
/// 底部导航是手机的拇指区布局。桌面上它占掉整整一条屏宽，却只放两个按钮，
/// 而聊天档案本身已经是左右分栏 —— 导航放到最左一列，三栏从左到右就是
/// 「去哪 → 选哪段 → 读这段」，与阅读方向一致。
class HomePage extends StatefulWidget {
  const HomePage({super.key});

  /// 与 [SessionsPage] 切成左右分栏的宽度一致：分栏出现时导航同时移到侧边。
  static const wideBreakpoint = 1000.0;

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  int _index = 0;

  static const _destinations = [
    (icon: Icons.search, selected: Icons.search, label: '记忆'),
    (icon: Icons.forum_outlined, selected: Icons.forum, label: '聊天档案'),
  ];

  @override
  Widget build(BuildContext context) {
    final activeIndex = _index.clamp(0, 1);
    final wide = MediaQuery.sizeOf(context).width >= HomePage.wideBreakpoint;
    final pages = IndexedStack(
      index: activeIndex,
      // `IndexedStack` 而不是按需构建：切回来时滚动位置与检索结果还在，
      // 而重建会把它们都丢掉 —— 在记忆浏览器里那意味着刚调好的回放点
      // 没了
      children: [
        TickerMode(enabled: activeIndex == 0, child: const MemoryBrowser()),
        const SessionsPage(),
      ],
    );

    if (wide) {
      final scheme = Theme.of(context).colorScheme;
      return Scaffold(
        body: Row(
          children: [
            NavigationRail(
              selectedIndex: activeIndex,
              onDestinationSelected: (i) => setState(() => _index = i),
              labelType: NavigationRailLabelType.all,
              backgroundColor: scheme.surfaceContainerLow,
              leading: Padding(
                padding: const EdgeInsets.only(top: 6, bottom: 18),
                child: Tooltip(
                  message: 'Cormex',
                  child: Icon(
                    Icons.psychology_rounded,
                    color: CormexTheme.memoryAccent(scheme.brightness),
                    size: 28,
                  ),
                ),
              ),
              destinations: [
                for (final d in _destinations)
                  NavigationRailDestination(
                    icon: Icon(d.icon),
                    selectedIcon: Icon(d.selected),
                    label: Text(d.label),
                  ),
              ],
            ),
            VerticalDivider(width: 1, color: scheme.outlineVariant),
            Expanded(child: pages),
          ],
        ),
      );
    }

    return Scaffold(
      body: pages,
      bottomNavigationBar: NavigationBar(
        selectedIndex: activeIndex,
        onDestinationSelected: (i) => setState(() => _index = i),
        destinations: [
          for (final d in _destinations)
            NavigationDestination(
              icon: Icon(d.icon),
              selectedIcon: Icon(d.selected),
              label: d.label,
            ),
        ],
      ),
    );
  }
}
