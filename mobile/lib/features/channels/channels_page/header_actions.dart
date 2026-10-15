part of '../channels_page.dart';

class _ChannelHeaderActions extends StatelessWidget {
  const _ChannelHeaderActions({
    required this.onSearch,
    required this.onSettings,
  });

  final VoidCallback? onSearch;
  final VoidCallback onSettings;

  @override
  Widget build(BuildContext context) => ClipRRect(
    borderRadius: BorderRadius.circular(Radii.full),
    child: ColoredBox(
      color: context.colors.primaryContainer,
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (onSearch != null)
            IconButton(
              tooltip: 'Search',
              icon: const Icon(BuzzIcons.search300),
              onPressed: onSearch,
            ),
          SizedBox(
            width: Grid.xl,
            height: Grid.xl,
            child: Center(
              child: ProfileAvatar(
                size: _kTopSectionProfileAvatarSize,
                showPresence: false,
                onTap: onSettings,
              ),
            ),
          ),
        ],
      ),
    ),
  );
}
