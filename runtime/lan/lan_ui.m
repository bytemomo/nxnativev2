// Local play's overlay on the game: while the game hosts or looks for rooms, a capsule at the top of the screen says how it goes at a glance (the room and
// its players, the rooms found, the room joined and its round trip, a friend's room that does not answer); tapped, it opens a panel (lan_nw.h's lan_nw_config,
// kept for this app in UserDefaults and applied at once with lan_nw_configure):
//   - hosting: the room (its name, renamed here; the players, how each is connected and its round trip), where it is shared (nearby: Wi-Fi and peer-to-peer;
//     VPN: this device's VPN addresses, each with Share: an invite to send with any app), the password and the port;
//   - looking: the rooms found (they are in the game's own list: the game joins them), where to look (nearby, friends), the friends by address with what each
//     answered, and Paste (Apple's paste button: no question) or an address typed in to add one.
// An invite is text with "nxlan:<host>:<port>/<key>" in it: the key is the room's (lan_nw_key: made from the game and the password, never the password itself),
// so the friend reaches the room whatever password their own device has. Friends are remembered with the host's name once a room of theirs was seen.
// When iOS denies Local Network access (lan_nw.m: Bonjour's policy denied), the capsule turns red and opens Settings instead, where the switch is
// (Privacy & Security › Local Network; in LiveContainer, LiveContainer's). Other platforms (the Mac host) have no overlay: environment variables.
#include <TargetConditionals.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lan_nw.h"
#if TARGET_OS_IOS
#import <UIKit/UIKit.h>
#import <UniformTypeIdentifiers/UniformTypeIdentifiers.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <arpa/inet.h>

static NSString *const kPassword = @"dm.lan.password", *const kPort = @"dm.lan.port", *const kNearby = @"dm.lan.nearby", *const kVpn = @"dm.lan.vpn",
                *const kLookFriends = @"dm.lan.friends.on", *const kRoom = @"dm.lan.room", *const kFriends = @"dm.lan.friends", *const kPeers = @"dm.lan.peers";
static _Atomic int g_blocked;   // Local Network access denied

// ---------- the settings ----------
static NSString *str(NSString *key, const char *env) {
    NSString *s = [NSUserDefaults.standardUserDefaults stringForKey:key];
    if (!s) { const char *e = getenv(env); s = e ? @(e) : @""; }
    return s;
}
static BOOL flag(NSString *key, const char *env) {
    NSUserDefaults *d = NSUserDefaults.standardUserDefaults;
    if ([d objectForKey:key]) return [d boolForKey:key];
    const char *e = getenv(env); return !(e && !strcmp(e, "0"));
}
// The friends: [{a: address, k: room key (absent: this device's own), n: the host's name once seen, t: when}]. The addresses typed in before friends
// existed (dm.lan.peers, DM_LAN_PEERS) become friends the first time
static NSArray<NSDictionary *> *friends_list(void) {
    NSArray *f = [NSUserDefaults.standardUserDefaults arrayForKey:kFriends];
    if (f) return f;
    NSMutableArray *out = [NSMutableArray array];
    for (NSString *raw in [str(kPeers, "DM_LAN_PEERS") componentsSeparatedByCharactersInSet:[NSCharacterSet characterSetWithCharactersInString:@", \n\t;"]]) {
        NSString *a = [raw stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceCharacterSet];
        if (a.length) [out addObject:@{@"a": a}];
    }
    return out;
}
static void friends_save(NSArray<NSDictionary *> *f) { [NSUserDefaults.standardUserDefaults setObject:f forKey:kFriends]; }
static void read_config(lan_nw_config *c) {
    NSUserDefaults *d = NSUserDefaults.standardUserDefaults;
    memset(c, 0, sizeof *c);
    snprintf(c->password, sizeof c->password, "%s", str(kPassword, "DM_LAN_PASSWORD").UTF8String ?: "");
    snprintf(c->room, sizeof c->room, "%s", str(kRoom, "DM_LAN_ROOM").UTF8String ?: "");
    c->port = [d objectForKey:kPort] ? (int)[d integerForKey:kPort] : (getenv("DM_LAN_PORT") ? atoi(getenv("DM_LAN_PORT")) : LAN_DEFAULT_PORT);
    if (c->port <= 0 || c->port > 65535) c->port = LAN_DEFAULT_PORT;
    c->nearby = flag(kNearby, "DM_LAN_NEARBY"); c->vpn = flag(kVpn, "DM_LAN_VPN"); c->friends = flag(kLookFriends, "DM_LAN_FRIENDS");
    for (NSDictionary *f in friends_list()) {
        if (c->nfriends >= LAN_FRIENDS_MAX) break;
        NSString *a = f[@"a"]; NSData *k = f[@"k"];
        if (![a isKindOfClass:NSString.class] || !a.length) continue;
        lan_nw_friend *o = &c->friend_list[c->nfriends++];
        snprintf(o->addr, sizeof o->addr, "%s", a.UTF8String);
        if ([k isKindOfClass:NSData.class] && k.length == LAN_KEY_LEN) { memcpy(o->key, k.bytes, LAN_KEY_LEN); o->has_key = 1; }
    }
}
static void apply(void) {   // the settings as they are now, to the transport and the core
    lan_nw_config c; read_config(&c);
    lan_nw_configure(&c);
    lan_ctx *x = lan_current(); if (x) lan_set_room_name(x, c.room);
}

// ---------- invites ----------
static NSString *b64url(NSData *d) {
    return [[[[d base64EncodedStringWithOptions:0] stringByReplacingOccurrencesOfString:@"+" withString:@"-"] stringByReplacingOccurrencesOfString:@"/" withString:@"_"]
            stringByReplacingOccurrencesOfString:@"=" withString:@""];
}
static NSData *unb64url(NSString *s) {
    NSMutableString *m = [[s stringByReplacingOccurrencesOfString:@"-" withString:@"+"] stringByReplacingOccurrencesOfString:@"_" withString:@"/"].mutableCopy;
    while (m.length % 4) [m appendString:@"="];
    return [[NSData alloc] initWithBase64EncodedString:m options:0];
}
// The friends in a text: each "nxlan:<host>:<port>[/<key>]" in it, or the whole text as an address ("host" or "host:port") when it has none
static NSArray<NSDictionary *> *parse_invites(NSString *text) {
    NSMutableArray *out = [NSMutableArray array];
    NSRegularExpression *re = [NSRegularExpression regularExpressionWithPattern:@"nxlan:(\\[[0-9A-Fa-f:.]+\\]|[A-Za-z0-9.\\-]+):([0-9]{1,5})(?:/([A-Za-z0-9_\\-]{43}))?" options:0 error:nil];
    for (NSTextCheckingResult *m in [re matchesInString:text options:0 range:NSMakeRange(0, text.length)]) {
        NSString *addr = [NSString stringWithFormat:@"%@:%@", [text substringWithRange:[m rangeAtIndex:1]], [text substringWithRange:[m rangeAtIndex:2]]];
        NSData *key = [m rangeAtIndex:3].location != NSNotFound ? unb64url([text substringWithRange:[m rangeAtIndex:3]]) : nil;
        [out addObject:key.length == LAN_KEY_LEN ? @{@"a": addr, @"k": key} : @{@"a": addr}];
    }
    NSString *t = [text stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceAndNewlineCharacterSet];
    if (!out.count && t.length && t.length < 200 && [t rangeOfCharacterFromSet:NSCharacterSet.whitespaceAndNewlineCharacterSet].location == NSNotFound) [out addObject:@{@"a": t}];
    return out;
}
static NSString *game_name(void) {
    NSDictionary *i = NSBundle.mainBundle.infoDictionary;
    NSString *n = i[@"CFBundleDisplayName"] ?: i[@"CFBundleName"];
    return [n isKindOfClass:NSString.class] && n.length && ![n isEqualToString:@"LiveContainer"] ? n : nil;
}

// This device's IPv4 addresses with what each is: @[@[kind, address]] (LAN_IF_WIFI, LAN_IF_WIRED, LAN_IF_VPN; cellular and link-local left out)
static NSArray<NSArray *> *interfaces(void) {
    struct ifaddrs *list = NULL;
    NSMutableArray *out = [NSMutableArray array];
    if (getifaddrs(&list) != 0) return out;
    for (struct ifaddrs *i = list; i; i = i->ifa_next) {
        if (!i->ifa_addr || i->ifa_addr->sa_family != AF_INET || !(i->ifa_flags & IFF_UP) || (i->ifa_flags & IFF_LOOPBACK)) continue;
        char ip[INET_ADDRSTRLEN]; inet_ntop(AF_INET, &((struct sockaddr_in *)i->ifa_addr)->sin_addr, ip, sizeof ip);
        if (!strncmp(ip, "169.254.", 8)) continue;
        const char *n = i->ifa_name;
        if (!strncmp(n, "pdp_ip", 6) || !strncmp(n, "awdl", 4) || !strncmp(n, "llw", 3)) continue;
        int kind = !strcmp(n, "en0") ? LAN_IF_WIFI : !strncmp(n, "en", 2) ? LAN_IF_WIRED : LAN_IF_VPN;
        [out addObject:@[@(kind), @(ip)]];
    }
    freeifaddrs(list);
    return out;
}

// ---------- the look: the launcher's (dark material, cards, SF Symbols) ----------
static UIColor *rgb(int hex) { return [UIColor colorWithRed:((hex >> 16) & 255) / 255.0 green:((hex >> 8) & 255) / 255.0 blue:(hex & 255) / 255.0 alpha:1]; }
#define C_OK rgb(0x34c759)
#define C_WARN rgb(0xff9f0a)
#define C_BAD rgb(0xff453a)
#define C_BLUE rgb(0x0a84ff)
#define C_PURPLE rgb(0xbf5af2)
#define C_MUTED [UIColor colorWithWhite:0.92 alpha:0.62]
#define C_FAINT [UIColor colorWithWhite:0.92 alpha:0.36]

static UILabel *label(NSString *t, CGFloat size, UIFontWeight w, UIColor *color) {
    UILabel *l = [UILabel new]; l.text = t; l.font = [UIFont systemFontOfSize:size weight:w]; l.textColor = color;
    l.adjustsFontForContentSizeCategory = NO; l.lineBreakMode = NSLineBreakByTruncatingTail;
    return l;
}
static UILabel *section(NSString *t) { UILabel *l = label(t.uppercaseString, 11, UIFontWeightSemibold, C_FAINT); return l; }
static UIView *fixed(UIView *v, CGFloat w, CGFloat h) {
    v.translatesAutoresizingMaskIntoConstraints = NO;
    if (w > 0) [v.widthAnchor constraintEqualToConstant:w].active = YES;
    if (h > 0) [v.heightAnchor constraintEqualToConstant:h].active = YES;
    return v;
}
static UIView *tile(NSString *symbol, UIColor *color) {
    UIView *t = fixed([UIView new], 26, 26); t.backgroundColor = color; t.layer.cornerRadius = 7;
    UIImageView *i = [[UIImageView alloc] initWithImage:[UIImage systemImageNamed:symbol withConfiguration:[UIImageSymbolConfiguration configurationWithPointSize:13 weight:UIImageSymbolWeightSemibold]]];
    i.tintColor = UIColor.whiteColor; i.translatesAutoresizingMaskIntoConstraints = NO; [t addSubview:i];
    [NSLayoutConstraint activateConstraints:@[[i.centerXAnchor constraintEqualToAnchor:t.centerXAnchor], [i.centerYAnchor constraintEqualToAnchor:t.centerYAnchor]]];
    return t;
}
static UIColor *avatar_color(NSString *name) {
    static const int palette[] = {0xff375f, 0x30d158, 0xff9f0a, 0x64d2ff, 0xbf5af2, 0x0a84ff, 0xffd60a, 0xac8e68};
    NSUInteger h = 0; for (NSUInteger i = 0; i < name.length; i++) h = h * 31 + [name characterAtIndex:i];
    return rgb(palette[h % 8]);
}
static UIView *avatar(NSString *name, UIColor *color) {
    UIView *a = fixed([UIView new], 26, 26); a.backgroundColor = color ?: avatar_color(name); a.layer.cornerRadius = 13;
    UILabel *l = label(name.length ? [name substringToIndex:[name rangeOfComposedCharacterSequenceAtIndex:0].length].uppercaseString : @"?", 12, UIFontWeightBold, UIColor.whiteColor);
    l.translatesAutoresizingMaskIntoConstraints = NO; [a addSubview:l];
    [NSLayoutConstraint activateConstraints:@[[l.centerXAnchor constraintEqualToAnchor:a.centerXAnchor], [l.centerYAnchor constraintEqualToAnchor:a.centerYAnchor]]];
    return a;
}
static UIView *badge(NSString *t, UIColor *tint) {   // tint nil: plain
    UILabel *l = label(t, 10.5, UIFontWeightSemibold, tint ? [tint colorWithAlphaComponent:1] : C_MUTED);
    UIView *b = [UIView new]; b.backgroundColor = tint ? [tint colorWithAlphaComponent:0.22] : [UIColor colorWithWhite:1 alpha:0.1]; b.layer.cornerRadius = 6;
    l.translatesAutoresizingMaskIntoConstraints = NO; [b addSubview:l];
    [NSLayoutConstraint activateConstraints:@[[l.leadingAnchor constraintEqualToAnchor:b.leadingAnchor constant:7], [l.trailingAnchor constraintEqualToAnchor:b.trailingAnchor constant:-7],
                                              [l.topAnchor constraintEqualToAnchor:b.topAnchor constant:2], [l.bottomAnchor constraintEqualToAnchor:b.bottomAnchor constant:-2]]];
    [b setContentCompressionResistancePriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
    [l setContentCompressionResistancePriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
    return b;
}
static UIView *ping(int ms) {
    if (ms < 0) return nil;
    NSMutableAttributedString *s = [[NSMutableAttributedString alloc] initWithString:@"● " attributes:@{NSForegroundColorAttributeName: ms < 80 ? C_OK : ms < 200 ? C_WARN : C_BAD,
                                                                                                   NSFontAttributeName: [UIFont systemFontOfSize:8]}];
    [s appendAttributedString:[[NSAttributedString alloc] initWithString:[NSString stringWithFormat:@"%d ms", ms] attributes:@{NSForegroundColorAttributeName: C_MUTED,
                                                                                                                           NSFontAttributeName: [UIFont monospacedDigitSystemFontOfSize:12 weight:UIFontWeightRegular]}]];
    UILabel *l = [UILabel new]; l.attributedText = s; [l setContentCompressionResistancePriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
    return l;
}
static NSArray<UIView *> *nonnil(UIView *__strong const *v, int n) { NSMutableArray *a = [NSMutableArray array]; for (int i = 0; i < n; i++) if (v[i]) [a addObject:v[i]]; return a; }
#define VIEWS(...) ({ UIView *__strong _v[] = {__VA_ARGS__}; nonnil(_v, (int)(sizeof _v / sizeof _v[0])); })   // an array literal throws on nil: the views there are
static UIStackView *hstack(NSArray<UIView *> *views, CGFloat spacing) {
    UIStackView *s = [[UIStackView alloc] initWithArrangedSubviews:views]; s.axis = UILayoutConstraintAxisHorizontal; s.alignment = UIStackViewAlignmentCenter; s.spacing = spacing; return s;
}
static UIStackView *vstack(NSArray<UIView *> *views, CGFloat spacing) {
    UIStackView *s = [[UIStackView alloc] initWithArrangedSubviews:views]; s.axis = UILayoutConstraintAxisVertical; s.spacing = spacing; return s;
}
// A row: something on the left (a tile or an avatar), a title over a line, and what is on the right
static UIView *row(UIView *lead, UIView *title, NSString *meta, NSArray<UIView *> *trail) {
    NSMutableArray *texts = [NSMutableArray arrayWithObject:title];
    if (meta.length) [texts addObject:label(meta, 11.5, UIFontWeightRegular, C_MUTED)];
    UIStackView *t = vstack(texts, 1);
    [t setContentHuggingPriority:UILayoutPriorityDefaultLow forAxis:UILayoutConstraintAxisHorizontal];
    [t setContentCompressionResistancePriority:UILayoutPriorityDefaultLow forAxis:UILayoutConstraintAxisHorizontal];
    NSMutableArray *all = [NSMutableArray array];
    if (lead) [all addObject:lead];
    [all addObject:t];
    for (UIView *v in trail) if (v) [all addObject:v];
    UIStackView *r = hstack(all, 9);
    r.layoutMargins = UIEdgeInsetsMake(6, 0, 6, 0); r.layoutMarginsRelativeArrangement = YES;
    [r.heightAnchor constraintGreaterThanOrEqualToConstant:42].active = YES;
    return r;
}
static UILabel *title_label(NSString *t) { return label(t, 14, UIFontWeightMedium, UIColor.whiteColor); }
// A card: rows on a lighter ground, with hairlines between them
static UIStackView *card(void) {
    UIStackView *c = vstack(@[], 0);
    c.layoutMargins = UIEdgeInsetsMake(4, 12, 4, 12); c.layoutMarginsRelativeArrangement = YES;
    UIView *bg = [UIView new]; bg.backgroundColor = [UIColor colorWithWhite:1 alpha:0.07]; bg.layer.cornerRadius = 13; bg.translatesAutoresizingMaskIntoConstraints = NO;
    [c insertSubview:bg atIndex:0];
    [NSLayoutConstraint activateConstraints:@[[bg.leadingAnchor constraintEqualToAnchor:c.leadingAnchor], [bg.trailingAnchor constraintEqualToAnchor:c.trailingAnchor],
                                              [bg.topAnchor constraintEqualToAnchor:c.topAnchor], [bg.bottomAnchor constraintEqualToAnchor:c.bottomAnchor]]];
    return c;
}
static void card_set(UIStackView *c, NSArray<UIView *> *rows) {
    for (UIView *v in c.arrangedSubviews.copy) { [c removeArrangedSubview:v]; [v removeFromSuperview]; }
    for (NSUInteger i = 0; i < rows.count; i++) {
        if (i) { UIView *line = fixed([UIView new], 0, 0.5); line.backgroundColor = [UIColor colorWithWhite:1 alpha:0.08]; [c addArrangedSubview:line]; }
        [c addArrangedSubview:rows[i]];
    }
}
static UIButton *button(NSString *t, NSString *symbol, BOOL primary, id target, SEL action) {
    UIButtonConfiguration *cfg = primary ? UIButtonConfiguration.filledButtonConfiguration : UIButtonConfiguration.grayButtonConfiguration;
    cfg.title = t; cfg.cornerStyle = UIButtonConfigurationCornerStyleMedium; cfg.buttonSize = UIButtonConfigurationSizeSmall;
    if (symbol) { cfg.image = [UIImage systemImageNamed:symbol]; cfg.imagePadding = 4; cfg.preferredSymbolConfigurationForImage = [UIImageSymbolConfiguration configurationWithPointSize:11 weight:UIImageSymbolWeightSemibold]; }
    if (primary) cfg.baseBackgroundColor = C_BLUE; else cfg.baseForegroundColor = UIColor.whiteColor;
    cfg.titleTextAttributesTransformer = ^NSDictionary *(NSDictionary *a) { NSMutableDictionary *m = a.mutableCopy; m[NSFontAttributeName] = [UIFont systemFontOfSize:12.5 weight:UIFontWeightSemibold]; return m; };
    UIButton *b = [UIButton buttonWithConfiguration:cfg primaryAction:nil];
    [b addTarget:target action:action forControlEvents:UIControlEventTouchUpInside];
    [b setContentCompressionResistancePriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
    [b setContentHuggingPriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
    return b;
}
static UIButton *link_button(NSString *t, id target, SEL action) {
    UIButton *b = [UIButton buttonWithType:UIButtonTypeSystem];
    [b setTitle:t forState:UIControlStateNormal]; b.titleLabel.font = [UIFont systemFontOfSize:13 weight:UIFontWeightMedium]; b.tintColor = C_BLUE;
    [b addTarget:target action:action forControlEvents:UIControlEventTouchUpInside];
    [b setContentCompressionResistancePriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
    return b;
}
static UISwitch *toggle(BOOL on, id target, SEL action) {
    UISwitch *s = [UISwitch new]; s.on = on; s.onTintColor = C_OK; [s addTarget:target action:action forControlEvents:UIControlEventValueChanged];
    [s setContentCompressionResistancePriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
    return s;
}
static NSString *kind_badge(int kind, UIColor **tint) {
    if (kind == LAN_IF_VPN) { *tint = C_PURPLE; return @"VPN"; }
    if (kind == LAN_IF_WIFI || kind == LAN_IF_P2P || kind == LAN_IF_WIRED) { *tint = C_BLUE; return @"Nearby"; }
    *tint = nil; return nil;
}
static NSString *text(const char *s) {   // names from other devices may not be UTF-8: never nil
    if (!s) return @"";
    return @(s) ?: [[NSString alloc] initWithCString:s encoding:NSISOLatin1StringEncoding] ?: @"";
}
static NSString *room_title(const char *room, const char *host) {   // a room's name, or its host's
    if (room && room[0]) return text(room);
    NSString *h = host && host[0] ? text(host) : nil;
    return h ? [NSString stringWithFormat:@"%@'s room", h] : @"A room";
}
static NSString *plural(int n, NSString *one) { return [NSString stringWithFormat:@"%d %@%@", n, one, n == 1 ? @"" : @"s"]; }
static NSString *when(NSTimeInterval t) {
    if (t <= 0) return @"not played yet";
    NSTimeInterval d = NSDate.date.timeIntervalSince1970 - t;
    return d < 86400 ? @"today" : d < 2 * 86400 ? @"yesterday" : d < 14 * 86400 ? [NSString stringWithFormat:@"%d days ago", (int)(d / 86400)] : [NSString stringWithFormat:@"%d weeks ago", (int)(d / 604800)];
}

// ---------- the overlay ----------
@interface DmLanOverlay : UIView <UITextFieldDelegate>
@property(nonatomic, strong) UIButton *pill;
@property(nonatomic, strong) UIVisualEffectView *panel;
@property(nonatomic, strong) UIScrollView *scroll;
@property(nonatomic, strong) UIStackView *columns, *left, *right;
@property(nonatomic) int mode;   // the panel built: 0 none, 1 hosting, 2 looking
@property(nonatomic) BOOL open;
// what refresh updates
@property(nonatomic, strong) UILabel *roomTitle, *roomLine, *listTitle;
@property(nonatomic, strong) UIStackView *list, *vpnRows, *friendRows;
@property(nonatomic, strong) UIView *lockBadge;
@property(nonatomic, strong) UISwitch *nearby, *vpn, *lookFriends;
@property(nonatomic, strong) UILabel *nearbyLine, *vpnLine, *passwordValue, *portValue;
@property(nonatomic, strong) UITextField *address;
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSString *> *shareAddr;   // a Share button's tag → the address it shares
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSString *> *removeAddr;  // a remove button's tag → the friend's address
@property(nonatomic, copy) NSString *vpnSig, *friendSig;   // what the rows with buttons show: rebuilt only when it changes (a tap must not lose its button)
@property(nonatomic, strong) NSTimer *timer;
@property(nonatomic) CGFloat keyboard;
@end

@implementation DmLanOverlay
- (instancetype)initWithFrame:(CGRect)frame {
    if (!(self = [super initWithFrame:frame])) return nil;
    self.backgroundColor = UIColor.clearColor;
    _shareAddr = [NSMutableDictionary dictionary]; _removeAddr = [NSMutableDictionary dictionary];
    _pill = [UIButton buttonWithType:UIButtonTypeCustom];
    _pill.backgroundColor = [UIColor colorWithWhite:0.11 alpha:0.78];
    _pill.layer.cornerRadius = 15; _pill.layer.shadowColor = UIColor.blackColor.CGColor; _pill.layer.shadowOpacity = 0.35; _pill.layer.shadowRadius = 8; _pill.layer.shadowOffset = CGSizeMake(0, 3);
    [_pill addTarget:self action:@selector(toggle) forControlEvents:UIControlEventTouchUpInside];
    [self addSubview:_pill];

    _panel = [[UIVisualEffectView alloc] initWithEffect:[UIBlurEffect effectWithStyle:UIBlurEffectStyleSystemThickMaterialDark]];
    _panel.layer.cornerRadius = 22; _panel.clipsToBounds = YES; _panel.hidden = YES; _panel.alpha = 0;
    _panel.layer.borderWidth = 0.5; _panel.layer.borderColor = [UIColor colorWithWhite:1 alpha:0.12].CGColor;
    _scroll = [UIScrollView new]; _scroll.alwaysBounceVertical = NO; _scroll.keyboardDismissMode = UIScrollViewKeyboardDismissModeInteractive;
    [_panel.contentView addSubview:_scroll];
    _left = vstack(@[], 8); _right = vstack(@[], 8);
    _columns = [[UIStackView alloc] initWithArrangedSubviews:@[_left, _right]];
    _columns.spacing = 14; _columns.alignment = UIStackViewAlignmentTop; _columns.distribution = UIStackViewDistributionFillEqually;
    _columns.translatesAutoresizingMaskIntoConstraints = NO;
    [_scroll addSubview:_columns];
    [NSLayoutConstraint activateConstraints:@[
        [_columns.leadingAnchor constraintEqualToAnchor:_scroll.contentLayoutGuide.leadingAnchor constant:14], [_columns.trailingAnchor constraintEqualToAnchor:_scroll.contentLayoutGuide.trailingAnchor constant:-14],
        [_columns.topAnchor constraintEqualToAnchor:_scroll.contentLayoutGuide.topAnchor constant:14], [_columns.bottomAnchor constraintEqualToAnchor:_scroll.contentLayoutGuide.bottomAnchor constant:-14],
        [_columns.widthAnchor constraintEqualToAnchor:_scroll.frameLayoutGuide.widthAnchor constant:-28]]];
    [self addSubview:_panel];

    self.pasteConfiguration = [[UIPasteConfiguration alloc] initWithAcceptableTypeIdentifiers:@[UTTypeUTF8PlainText.identifier, UTTypePlainText.identifier, UTTypeURL.identifier]];
    [NSNotificationCenter.defaultCenter addObserver:self selector:@selector(keyboardChanged:) name:UIKeyboardWillChangeFrameNotification object:nil];
    [self refresh];
    return self;
}
- (void)dealloc { [NSNotificationCenter.defaultCenter removeObserver:self]; [_timer invalidate]; }
- (void)didMoveToWindow {
    [super didMoveToWindow];
    [_timer invalidate]; _timer = nil;
    if (self.window) _timer = [NSTimer scheduledTimerWithTimeInterval:1 target:self selector:@selector(refresh) userInfo:nil repeats:YES];
}
- (UIView *)hitTest:(CGPoint)p withEvent:(UIEvent *)e {   // only the capsule and the open panel take touches; the game gets the rest
    UIView *v = [super hitTest:p withEvent:e];
    return v == self ? nil : v;
}

// ---------- layout ----------
- (void)layoutSubviews {
    [super layoutSubviews];
    UIEdgeInsets safe = self.safeAreaInsets;
    CGFloat W = self.bounds.size.width, H = self.bounds.size.height;
    CGSize ps = [_pill sizeThatFits:CGSizeMake(W - 40, 30)];
    CGFloat pw = MIN(MAX(ps.width + 26, 120), W - 40);
    _pill.frame = CGRectMake(floor((W - pw) / 2), safe.top + 8, pw, 30);
    if (_panel.hidden) return;
    CGFloat w = MIN(660, W - MAX(safe.left, 16) - MAX(safe.right, 16));
    BOOL two = w >= 520;   // side by side in landscape; one under the other when narrow
    _columns.axis = two ? UILayoutConstraintAxisHorizontal : UILayoutConstraintAxisVertical;
    _columns.distribution = two ? UIStackViewDistributionFillEqually : UIStackViewDistributionFill;
    CGFloat top = CGRectGetMaxY(_pill.frame) + 8, bottom = MAX(safe.bottom, 8);
    CGFloat avail = MAX(H - top - bottom - (_keyboard > 0 ? MAX(0, _keyboard - bottom) : 0), 120);
    _panel.frame = CGRectMake(floor((W - w) / 2), top, w, avail);   // laid out at full height first: the content's height decides the panel's
    _scroll.frame = _panel.contentView.bounds;
    [_scroll layoutIfNeeded];
    CGFloat h = MIN(_scroll.contentSize.height, avail);
    _panel.frame = CGRectMake(floor((W - w) / 2), top, w, h);
    _scroll.frame = _panel.contentView.bounds;
}
- (void)keyboardChanged:(NSNotification *)n {
    CGRect end = [n.userInfo[UIKeyboardFrameEndUserInfoKey] CGRectValue];
    CGRect mine = [self convertRect:end fromView:nil];
    _keyboard = MAX(0, CGRectGetMaxY(self.bounds) - CGRectGetMinY(mine));
    [self setNeedsLayout];
    [UIView animateWithDuration:0.25 animations:^{ [self layoutIfNeeded]; } completion:^(BOOL done) {
        (void)done; if (self->_address.isFirstResponder) [self->_scroll scrollRectToVisible:[self->_scroll convertRect:self->_address.bounds fromView:self->_address] animated:YES];
    }];
}

// ---------- the capsule ----------
- (void)pillDot:(UIColor *)dot main:(NSString *)main sub:(NSString *)sub red:(BOOL)red {
    NSMutableAttributedString *s = [NSMutableAttributedString new];
    if (dot) [s appendAttributedString:[[NSAttributedString alloc] initWithString:@"● " attributes:@{NSForegroundColorAttributeName: dot, NSFontAttributeName: [UIFont systemFontOfSize:9]}]];
    [s appendAttributedString:[[NSAttributedString alloc] initWithString:main attributes:@{NSForegroundColorAttributeName: UIColor.whiteColor, NSFontAttributeName: [UIFont systemFontOfSize:13 weight:UIFontWeightSemibold]}]];
    if (sub.length) [s appendAttributedString:[[NSAttributedString alloc] initWithString:[@"  ·  " stringByAppendingString:sub]
                                                                              attributes:@{NSForegroundColorAttributeName: red ? UIColor.whiteColor : C_MUTED, NSFontAttributeName: [UIFont systemFontOfSize:13 weight:UIFontWeightMedium]}]];
    if (!red) [s appendAttributedString:[[NSAttributedString alloc] initWithString:self.open ? @"  ▴" : @"  ▾" attributes:@{NSForegroundColorAttributeName: C_MUTED, NSFontAttributeName: [UIFont systemFontOfSize:10]}]];
    [_pill setAttributedTitle:s forState:UIControlStateNormal];
    _pill.backgroundColor = red ? [UIColor colorWithRed:0.75 green:0.12 blue:0.12 alpha:0.9] : [UIColor colorWithWhite:0.11 alpha:0.78];
    [self setNeedsLayout];
}

// ---------- refresh: the capsule always, the panel when open ----------
- (void)refresh {
    if (g_blocked) { [self pillDot:nil main:@"⚠︎ Local Network is off" sub:@"Turn on" red:YES]; if (self.open) [self close]; return; }
    lan_ctx *x = lan_current();
    lan_status st; memset(&st, 0, sizeof st);
    if (x) lan_status_get(x, &st);
    lan_nw_config cfg; read_config(&cfg);
    lan_nw_friend_state fs[LAN_FRIENDS_MAX]; int nfs = lan_nw_friends(fs, LAN_FRIENDS_MAX);
    [self learnNames:&st friends:fs count:nfs];
    NSString *mine = st.nplayers ? room_title(cfg.room, st.players[0].name) : room_title(cfg.room, NULL);

    if (st.state == LDN_AP_CREATED) [self pillDot:C_OK main:mine sub:plural(st.nplayers, @"player") red:NO];
    else if (st.state == LDN_AP) [self pillDot:C_OK main:@"Opening a room" sub:nil red:NO];
    else if (st.state == LDN_STATION_CONNECTED) {
        int rtt = st.nplayers ? st.players[0].rtt_ms : -1; NSString *in = @"a room";
        for (int i = 0; i < st.nrooms; i++) if (st.rooms[i].joined) in = room_title(st.rooms[i].room, st.rooms[i].host);
        [self pillDot:C_OK main:[@"In " stringByAppendingString:in] sub:rtt >= 0 ? [NSString stringWithFormat:@"%d ms", rtt] : nil red:NO];
    } else {
        NSString *trouble = nil; NSString *who = nil;
        if (!st.nrooms) for (int i = 0; i < nfs; i++) {
            NSString *t = fs[i].state == LAN_FRIEND_NO_ANSWER ? @"no answer" : fs[i].state == LAN_FRIEND_KEY_DIFFERS ? @"other password or game" : fs[i].state == LAN_FRIEND_BAD_ADDRESS ? @"not an address" : nil;
            if (t) { trouble = t; who = [self friendName:text(fs[i].addr)]; break; }
        }
        if (trouble) [self pillDot:C_WARN main:who sub:trouble red:NO];
        else [self pillDot:rgb(0x0a84ff) main:@"Searching" sub:st.nrooms ? [NSString stringWithFormat:@"%@ found", plural(st.nrooms, @"room")] : @"no rooms yet" red:NO];
    }
    if (!self.open) return;
    int mode = st.state == LDN_AP || st.state == LDN_AP_CREATED ? 1 : 2;
    if (mode != _mode) [self build:mode];
    if (mode == 1) [self updateHosting:&st config:&cfg title:mine];
    else [self updateLooking:&st config:&cfg friends:fs count:nfs];
    [self setNeedsLayout];
}
- (NSString *)friendName:(NSString *)addr {
    for (NSDictionary *f in friends_list()) if ([f[@"a"] isEqualToString:addr]) { NSString *n = f[@"n"]; return [n isKindOfClass:NSString.class] && n.length ? n : addr; }
    return addr;
}
// A friend's room seen: the friend gets the host's name and the day
- (void)learnNames:(const lan_status *)st friends:(const lan_nw_friend_state *)fs count:(int)nfs {
    NSMutableArray *list = nil;
    for (int i = 0; i < nfs; i++) {
        if (!fs[i].service) continue;
        for (int r = 0; r < st->nrooms; r++) {
            if (st->rooms[r].service != fs[i].service || !st->rooms[r].host[0]) continue;
            if (!list) list = friends_list().mutableCopy;
            for (NSUInteger k = 0; k < list.count; k++) {
                NSDictionary *f = list[k];
                if (![f[@"a"] isEqualToString:text(fs[i].addr)]) continue;
                NSString *name = text(st->rooms[r].host); NSTimeInterval now = NSDate.date.timeIntervalSince1970;
                if ([f[@"n"] isEqual:name] && now - [f[@"t"] doubleValue] < 3600) continue;
                NSMutableDictionary *m = f.mutableCopy; m[@"n"] = name; m[@"t"] = @(now); list[k] = m;
            }
        }
    }
    if (list && ![list isEqualToArray:friends_list()]) friends_save(list);
}

// ---------- the panels ----------
- (void)build:(int)mode {
    _mode = mode;
    for (UIStackView *s in @[_left, _right]) for (UIView *v in s.arrangedSubviews.copy) { [s removeArrangedSubview:v]; [v removeFromSuperview]; }
    _list = card(); _vpnRows = nil; _friendRows = nil; _vpnSig = nil; _friendSig = nil;
    lan_nw_config cfg; read_config(&cfg);
    UIStackView *settings = card();
    _passwordValue = label(@"", 12, UIFontWeightRegular, C_MUTED); _portValue = label(@"", 12, UIFontWeightRegular, C_MUTED);
    NSMutableArray *srows = [NSMutableArray arrayWithObject:row(nil, title_label(@"Password"), nil, @[_passwordValue, link_button(@"Change", self, @selector(changePassword))])];
    if (mode == 1) {
        // left: the room and its players
        _roomTitle = label(@"", 15, UIFontWeightSemibold, UIColor.whiteColor); _roomLine = label(@"", 11.5, UIFontWeightRegular, C_MUTED);
        _lockBadge = badge(@"Password", nil);
        UIStackView *head = card();
        UIStackView *texts = vstack(@[hstack(@[_roomTitle, link_button(@"Rename", self, @selector(rename))], 6), _roomLine], 1);
        card_set(head, @[hstack(@[tile(@"dot.radiowaves.left.and.right", C_OK), texts, [UIView new], _lockBadge], 9)]);
        head.layoutMargins = UIEdgeInsetsMake(10, 12, 10, 12);
        _listTitle = section(@"Players");
        for (UIView *v in @[head, _listTitle, _list]) [_left addArrangedSubview:v];
        // right: where it is shared, then the settings
        _nearby = toggle(cfg.nearby, self, @selector(setNearby:)); _vpn = toggle(cfg.vpn, self, @selector(setVpn:));
        _nearbyLine = label(@"", 11.5, UIFontWeightRegular, C_MUTED); _vpnLine = label(@"", 11.5, UIFontWeightRegular, C_MUTED);
        UIStackView *share = card();
        _vpnRows = vstack(@[], 0);
        card_set(share, @[row(tile(@"wifi", C_BLUE), vstack(@[title_label(@"Nearby"), _nearbyLine], 1), nil, @[_nearby]),
                          row(tile(@"lock.shield.fill", C_PURPLE), vstack(@[title_label(@"VPN"), _vpnLine], 1), nil, @[_vpn])]);
        [share addArrangedSubview:_vpnRows];   // the VPN addresses, under the VPN row
        UILabel *note = label(@"Share sends the address and the room's key as text: a friend pastes it once.", 11.5, UIFontWeightRegular, C_MUTED); note.numberOfLines = 0;
        [srows addObject:row(nil, title_label(@"Port"), nil, @[_portValue, link_button(@"Change", self, @selector(changePort))])];
        card_set(settings, srows);
        for (UIView *v in @[section(@"Share on"), share, note, section(@"Settings"), settings]) [_right addArrangedSubview:v];
    } else {
        // left: the rooms found (or the players of the room joined), where to look
        _listTitle = section(@"Rooms found");
        UILabel *note = label(@"They are in the game's own list too: join from there.", 11.5, UIFontWeightRegular, C_MUTED); note.numberOfLines = 0;
        _nearby = toggle(cfg.nearby, self, @selector(setNearby:)); _lookFriends = toggle(cfg.friends, self, @selector(setLookFriends:));
        UIStackView *look = card();
        card_set(look, @[row(tile(@"wifi", C_BLUE), title_label(@"Nearby"), nil, @[_nearby]), row(tile(@"person.2.fill", C_PURPLE), title_label(@"Friends by address"), nil, @[_lookFriends])]);
        for (UIView *v in @[_listTitle, _list, note, section(@"Look on"), look]) [_left addArrangedSubview:v];
        // right: the friends, adding one, the password
        _friendRows = card();
        _address = [UITextField new]; _address.delegate = self; _address.textColor = UIColor.whiteColor; _address.font = [UIFont systemFontOfSize:13.5];
        _address.attributedPlaceholder = [[NSAttributedString alloc] initWithString:@"Address (host or host:port)" attributes:@{NSForegroundColorAttributeName: C_FAINT}];
        _address.keyboardType = UIKeyboardTypeURL; _address.autocapitalizationType = UITextAutocapitalizationTypeNone; _address.autocorrectionType = UITextAutocorrectionTypeNo;
        _address.spellCheckingType = UITextSpellCheckingTypeNo; _address.returnKeyType = UIReturnKeyDone; _address.clearButtonMode = UITextFieldViewModeWhileEditing;
        [_address setContentHuggingPriority:UILayoutPriorityDefaultLow forAxis:UILayoutConstraintAxisHorizontal];
        UIPasteControlConfiguration *pcfg = [UIPasteControlConfiguration new];
        pcfg.displayMode = UIPasteControlDisplayModeIconAndLabel; pcfg.cornerStyle = UIButtonConfigurationCornerStyleCapsule;
        pcfg.baseBackgroundColor = UIColor.whiteColor; pcfg.baseForegroundColor = UIColor.blackColor;
        UIPasteControl *paste = [[UIPasteControl alloc] initWithConfiguration:pcfg]; paste.target = self;
        [paste setContentCompressionResistancePriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
        UIStackView *add = card();
        card_set(add, @[hstack(@[fixed(paste, 0, 30), _address, button(@"Add", nil, YES, self, @selector(addTyped))], 8)]);
        add.layoutMargins = UIEdgeInsetsMake(7, 8, 7, 8);
        UILabel *hint = label(@"Copied an invite? Paste adds the room.", 11.5, UIFontWeightRegular, C_MUTED); hint.numberOfLines = 0;
        card_set(settings, srows);
        for (UIView *v in @[section(@"Friends by address"), _friendRows, add, hint, section(@"Settings"), settings]) [_right addArrangedSubview:v];
    }
}
- (void)updateSettings:(const lan_nw_config *)cfg {
    _passwordValue.text = cfg->password[0] ? @"set" : @"none";
    int now = lan_nw_port();
    _portValue.text = now && now != cfg->port ? [NSString stringWithFormat:@"%d (%d was taken)", now, cfg->port] : [NSString stringWithFormat:@"%d", cfg->port];
    if (!_nearby.tracking) _nearby.on = cfg->nearby != 0;
    if (_vpn && !_vpn.tracking) _vpn.on = cfg->vpn != 0;
    if (_lookFriends && !_lookFriends.tracking) _lookFriends.on = cfg->friends != 0;
}
- (void)updateHosting:(const lan_status *)st config:(const lan_nw_config *)cfg title:(NSString *)title {
    [self updateSettings:cfg];
    _roomTitle.text = title;
    _roomLine.text = st->state == LDN_AP_CREATED ? plural(st->nplayers, @"player") : @"opening…";
    _lockBadge.hidden = !cfg->password[0];
    NSMutableArray *rows = [NSMutableArray array];
    for (int i = 0; i < st->nplayers; i++) {
        const lan_status_player *p = &st->players[i];
        NSString *name = text(p->name).length ? text(p->name) : @"Player";
        lan_nw_peer info; UIColor *tint = nil; NSString *b = nil, *meta = p->me ? @"This device" : @"";
        if (!p->me && lan_nw_peer_info(p->peer, &info)) { b = kind_badge(info.kind, &tint); meta = info.kind == LAN_IF_P2P ? @"peer-to-peer" : @(info.addr); }
        [rows addObject:row(avatar(name, p->me ? C_BLUE : nil), title_label(name), meta, VIEWS(p->me ? badge(@"Host", nil) : nil, b ? badge(b, tint) : nil, p->me ? nil : ping(p->rtt_ms)))];
    }
    if (!rows.count) [rows addObject:row(nil, label(@"Opening the room…", 13, UIFontWeightRegular, C_MUTED), nil, @[])];
    card_set(_list, rows);
    // nearby: this device's Wi-Fi address and peer-to-peer; VPN: an address per VPN, each with Share
    NSString *wifi = nil; NSMutableArray *vpns = [NSMutableArray array];
    for (NSArray *i in interfaces()) { int k = [i[0] intValue]; if (k == LAN_IF_WIFI || k == LAN_IF_WIRED) { if (!wifi) wifi = i[1]; } else [vpns addObject:i[1]]; }
    int port = lan_nw_port() ?: cfg->port;
    _nearbyLine.text = wifi ? [NSString stringWithFormat:@"Wi-Fi %@ · peer-to-peer", wifi] : @"peer-to-peer (Wi-Fi off)";
    _vpnLine.text = !vpns.count ? @"no VPN connected" : cfg->vpn ? (vpns.count == 1 ? @"friends add this address:" : @"friends add one of these addresses:") : @"off: friends on a VPN cannot join";
    NSString *sig = cfg->vpn ? [NSString stringWithFormat:@"%@:%d", [vpns componentsJoinedByString:@","], port] : @"";
    if ([sig isEqualToString:_vpnSig]) return;
    _vpnSig = sig;
    [_shareAddr removeAllObjects];
    NSMutableArray *vrows = [NSMutableArray array];
    if (cfg->vpn) for (NSUInteger i = 0; i < vpns.count; i++) {
        NSString *a = [NSString stringWithFormat:@"%@:%d", vpns[i], port];
        UIButton *share = button(@"Share", @"square.and.arrow.up", NO, self, @selector(share:)); share.tag = (NSInteger)i + 1; _shareAddr[@(share.tag)] = a;
        UILabel *l = label(a, 13, UIFontWeightMedium, UIColor.whiteColor); l.font = [UIFont monospacedDigitSystemFontOfSize:13 weight:UIFontWeightMedium];
        [vrows addObject:row(fixed([UIView new], 26, 1), l, nil, @[share])];
    }
    for (UIView *v in _vpnRows.arrangedSubviews.copy) { [_vpnRows removeArrangedSubview:v]; [v removeFromSuperview]; }
    for (UIView *v in vrows) [_vpnRows addArrangedSubview:v];
    _vpnRows.hidden = !vrows.count;
}
- (void)updateLooking:(const lan_status *)st config:(const lan_nw_config *)cfg friends:(const lan_nw_friend_state *)fs count:(int)nfs {
    [self updateSettings:cfg];
    NSMutableArray *rows = [NSMutableArray array];
    if (st->state == LDN_STATION_CONNECTED) {   // in a room: its players
        _listTitle.text = @"IN THE ROOM";
        for (int i = 0; i < st->nplayers; i++) {
            const lan_status_player *p = &st->players[i];
            NSString *name = text(p->name).length ? text(p->name) : @"Player";
            lan_nw_peer info; UIColor *tint = nil; NSString *b = nil;
            if (p->peer && lan_nw_peer_info(p->peer, &info)) b = kind_badge(info.kind, &tint);
            [rows addObject:row(avatar(name, p->me ? C_BLUE : nil), title_label(name), p->me ? @"This device" : i == 0 ? @"Host" : nil, VIEWS(b ? badge(b, tint) : nil, ping(p->rtt_ms)))];
        }
    } else {
        _listTitle.text = @"ROOMS FOUND";
        for (int i = 0; i < st->nrooms; i++) {
            const lan_status_room *r = &st->rooms[i];
            lan_nw_peer info; UIColor *tint = nil; NSString *b = nil;
            if (lan_nw_peer_info(r->peer, &info)) b = kind_badge(info.kind, &tint);
            [rows addObject:row(avatar(text(r->host), nil), title_label(room_title(r->room, r->host)), [NSString stringWithFormat:@"%d of %d players", r->count, r->max > 0 ? r->max : 8],
                                VIEWS(b ? badge(b, tint) : nil, ping(r->rtt_ms)))];
        }
        if (!rows.count) [rows addObject:row(nil, label(cfg->nearby || cfg->friends ? @"Looking…" : @"Nearby and friends are off: nothing is looked for", 13, UIFontWeightRegular, C_MUTED), nil, @[])];
    }
    card_set(_list, rows);
    // the friends, with what each answered
    NSArray *list = friends_list();
    NSMutableString *sig = [NSMutableString stringWithFormat:@"%d|", cfg->friends];
    for (NSDictionary *f in list) {
        int state = LAN_FRIEND_IDLE; for (int k = 0; k < nfs; k++) if ([text(fs[k].addr) isEqualToString:f[@"a"]]) state = fs[k].state;
        [sig appendFormat:@"%@/%@/%d/%@;", f[@"a"], f[@"n"], state, when([f[@"t"] doubleValue])];
    }
    if ([sig isEqualToString:_friendSig]) return;
    _friendSig = sig;
    NSMutableArray *frows = [NSMutableArray array];
    [_removeAddr removeAllObjects];
    for (NSUInteger i = 0; i < list.count; i++) {
        NSString *a = list[i][@"a"], *n = list[i][@"n"]; if (![a isKindOfClass:NSString.class]) continue;
        int state = LAN_FRIEND_IDLE;
        for (int k = 0; k < nfs; k++) if ([text(fs[k].addr) isEqualToString:a]) state = fs[k].state;
        BOOL named = [n isKindOfClass:NSString.class] && n.length;
        NSString *text; UIColor *color = C_MUTED; BOOL spin = NO;
        switch (state) {
            case LAN_FRIEND_TRYING: text = @"trying"; spin = YES; break;
            case LAN_FRIEND_HOSTING: text = @"hosting"; color = C_OK; break;
            case LAN_FRIEND_NOT_HOSTING: text = @"not hosting"; break;
            case LAN_FRIEND_NO_ANSWER: text = @"no answer"; color = C_WARN; break;
            case LAN_FRIEND_KEY_DIFFERS: text = @"other password or game"; color = C_WARN; break;
            case LAN_FRIEND_BAD_ADDRESS: text = @"not an address"; color = C_BAD; break;
            default: text = cfg->friends ? @"" : @"not looked for"; color = C_FAINT; break;
        }
        UILabel *sl = label(text, 12, UIFontWeightMedium, color); [sl setContentCompressionResistancePriority:UILayoutPriorityRequired forAxis:UILayoutConstraintAxisHorizontal];
        UIActivityIndicatorView *sp = nil;
        if (spin) { sp = [[UIActivityIndicatorView alloc] initWithActivityIndicatorStyle:UIActivityIndicatorViewStyleMedium]; sp.color = UIColor.whiteColor; sp.transform = CGAffineTransformMakeScale(0.7, 0.7); [sp startAnimating]; }
        UIButton *rm = [UIButton buttonWithType:UIButtonTypeSystem];
        [rm setImage:[UIImage systemImageNamed:@"xmark.circle.fill"] forState:UIControlStateNormal]; rm.tintColor = C_FAINT; rm.tag = (NSInteger)i + 1; _removeAddr[@(rm.tag)] = a;
        [rm addTarget:self action:@selector(removeFriend:) forControlEvents:UIControlEventTouchUpInside];
        NSString *meta = [NSString stringWithFormat:@"%@%@%@", named ? a : @"", named ? @" · " : @"", when([list[i][@"t"] doubleValue])];
        [frows addObject:row(avatar(named ? n : @"?", named ? nil : rgb(0x8e8e93)), title_label(named ? n : a), meta, VIEWS(sp, sl, rm))];
    }
    if (!frows.count) [frows addObject:row(nil, label(@"None yet: paste an invite, or type an address below.", 13, UIFontWeightRegular, C_MUTED), nil, @[])];
    card_set(_friendRows, frows);
}

// ---------- actions ----------
- (void)toggle {
    if (g_blocked) { [UIApplication.sharedApplication openURL:[NSURL URLWithString:UIApplicationOpenSettingsURLString] options:@{} completionHandler:nil]; return; }
    if (self.open) [self close]; else [self show];
}
- (void)show {
    self.open = YES; self.alpha = 1; _mode = 0;
    [NSObject cancelPreviousPerformRequestsWithTarget:self selector:@selector(fade) object:nil];
    _panel.hidden = NO;
    [self refresh]; [self layoutIfNeeded];
    [UIView animateWithDuration:0.18 animations:^{ self->_panel.alpha = 1; }];
}
- (void)close {
    if (!self.open) return;
    self.open = NO;
    [self endEditing:YES];
    [self refresh];
    [UIView animateWithDuration:0.18 animations:^{ self->_panel.alpha = 0; } completion:^(BOOL done) { (void)done; if (!self.open) self->_panel.hidden = YES; }];
    [self dim];
}
- (void)dim {   // out of the way while playing: fainter after a few seconds
    [NSObject cancelPreviousPerformRequestsWithTarget:self selector:@selector(fade) object:nil];
    [self performSelector:@selector(fade) withObject:nil afterDelay:4];
}
- (void)fade { if (!self.open && !g_blocked) [UIView animateWithDuration:0.4 animations:^{ self.alpha = 0.45; }]; }
- (void)wake { [NSObject cancelPreviousPerformRequestsWithTarget:self selector:@selector(fade) object:nil]; self.alpha = 1; if (!self.open) [self dim]; }

- (void)setNearby:(UISwitch *)s { [NSUserDefaults.standardUserDefaults setBool:s.on forKey:kNearby]; apply(); [self refresh]; }
- (void)setVpn:(UISwitch *)s { [NSUserDefaults.standardUserDefaults setBool:s.on forKey:kVpn]; apply(); [self refresh]; }
- (void)setLookFriends:(UISwitch *)s { [NSUserDefaults.standardUserDefaults setBool:s.on forKey:kLookFriends]; apply(); [self refresh]; }

- (UIViewController *)presenter {
    UIViewController *vc = self.window.rootViewController;
    while (vc.presentedViewController) vc = vc.presentedViewController;
    return vc;
}
- (void)ask:(NSString *)title message:(NSString *)msg value:(NSString *)value placeholder:(NSString *)ph secure:(BOOL)secure number:(BOOL)number done:(void (^)(NSString *))done {
    UIAlertController *a = [UIAlertController alertControllerWithTitle:title message:msg preferredStyle:UIAlertControllerStyleAlert];
    [a addTextFieldWithConfigurationHandler:^(UITextField *f) {
        f.text = value; f.placeholder = ph; f.secureTextEntry = secure; f.keyboardType = number ? UIKeyboardTypeNumberPad : UIKeyboardTypeDefault;
        f.autocapitalizationType = UITextAutocapitalizationTypeNone; f.autocorrectionType = UITextAutocorrectionTypeNo; f.clearButtonMode = UITextFieldViewModeWhileEditing;
    }];
    [a addAction:[UIAlertAction actionWithTitle:@"Cancel" style:UIAlertActionStyleCancel handler:nil]];
    __weak UIAlertController *wa = a;
    [a addAction:[UIAlertAction actionWithTitle:@"Save" style:UIAlertActionStyleDefault handler:^(UIAlertAction *x) { (void)x; done(wa.textFields.firstObject.text ?: @""); }]];
    [[self presenter] presentViewController:a animated:YES completion:nil];
}
- (void)changePassword {
    lan_nw_config c; read_config(&c);
    [self ask:@"Password" message:@"The same on every device that plays together (an invite carries the key made from it). Empty: any device with this game can join."
        value:@(c.password) placeholder:@"None" secure:YES number:NO done:^(NSString *v) {
        [NSUserDefaults.standardUserDefaults setObject:v forKey:kPassword]; apply(); [self refresh];
    }];
}
- (void)changePort {
    lan_nw_config c; read_config(&c);
    [self ask:@"Port" message:[NSString stringWithFormat:@"The port a room listens on, for friends who add it by address (%d unless set).", LAN_DEFAULT_PORT]
        value:[NSString stringWithFormat:@"%d", c.port] placeholder:[NSString stringWithFormat:@"%d", LAN_DEFAULT_PORT] secure:NO number:YES done:^(NSString *v) {
        int p = v.intValue; if (p <= 0 || p > 65535) p = LAN_DEFAULT_PORT;
        [NSUserDefaults.standardUserDefaults setInteger:p forKey:kPort]; apply(); [self refresh];
    }];
}
- (void)rename {
    lan_nw_config c; read_config(&c);
    [self ask:@"Room name" message:@"What others see. Empty: your player name's room." value:@(c.room) placeholder:@"Your player name's room" secure:NO number:NO done:^(NSString *v) {
        NSString *t = [v stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceAndNewlineCharacterSet];
        while ([t lengthOfBytesUsingEncoding:NSUTF8StringEncoding] >= LAN_ROOM_MAX) t = [t substringToIndex:t.length - 1];
        [NSUserDefaults.standardUserDefaults setObject:t forKey:kRoom]; apply(); [self refresh];
    }];
}
- (void)share:(UIButton *)b {
    NSString *addr = _shareAddr[@(b.tag)]; if (!addr) return;
    uint8_t key[LAN_KEY_LEN]; lan_nw_key(key);
    lan_nw_config c; read_config(&c);
    lan_status st; memset(&st, 0, sizeof st); lan_ctx *x = lan_current(); if (x) lan_status_get(x, &st);
    NSString *room = room_title(c.room, st.nplayers ? st.players[0].name : NULL), *game = game_name();
    NSString *text = [NSString stringWithFormat:@"Join %@%@%@. In the game's local play panel, tap Paste.\nnxlan:%@/%@", room, game ? @" in " : @"", game ?: @"", addr,
                      b64url([NSData dataWithBytes:key length:LAN_KEY_LEN])];
    UIActivityViewController *a = [[UIActivityViewController alloc] initWithActivityItems:@[text] applicationActivities:nil];
    a.popoverPresentationController.sourceView = b; a.popoverPresentationController.sourceRect = b.bounds;
    [[self presenter] presentViewController:a animated:YES completion:nil];
}
- (void)addFriends:(NSArray<NSDictionary *> *)found {
    if (!found.count) { [self flash:@"Nothing to add: no invite or address in it"]; return; }
    NSMutableArray *list = friends_list().mutableCopy;
    for (NSDictionary *f in found) {
        NSUInteger at = [list indexOfObjectPassingTest:^BOOL(NSDictionary *o, NSUInteger i, BOOL *stop) { (void)i; (void)stop; return [o[@"a"] isEqualToString:f[@"a"]]; }];
        if (at != NSNotFound) { NSMutableDictionary *m = [list[at] mutableCopy]; if (f[@"k"]) m[@"k"] = f[@"k"]; else [m removeObjectForKey:@"k"]; list[at] = m; }
        else {
            if (list.count >= LAN_FRIENDS_MAX) {   // full: the friend seen longest ago makes room
                NSUInteger oldest = 0; for (NSUInteger i = 1; i < list.count; i++) if ([list[i][@"t"] doubleValue] < [list[oldest][@"t"] doubleValue]) oldest = i;
                [list removeObjectAtIndex:oldest];
            }
            [list addObject:f];
        }
    }
    friends_save(list);
    [NSUserDefaults.standardUserDefaults setBool:YES forKey:kLookFriends];
    apply();
    [self flash:found.count == 1 ? [NSString stringWithFormat:@"Added %@: looking for its room", found[0][@"a"]] : [NSString stringWithFormat:@"Added %lu rooms", (unsigned long)found.count]];
    [self refresh];
}
- (void)pasteItemProviders:(NSArray<NSItemProvider *> *)providers {   // Apple's paste button (UIPasteControl): the text pasted, without a question
    for (NSItemProvider *p in providers) {
        if (![p canLoadObjectOfClass:NSString.class]) continue;
        [p loadObjectOfClass:NSString.class completionHandler:^(id<NSItemProviderReading> obj, NSError *err) {
            (void)err; NSString *s = [(NSObject *)obj isKindOfClass:NSString.class] ? (NSString *)obj : nil;
            dispatch_async(dispatch_get_main_queue(), ^{ [self addFriends:s ? parse_invites(s) : @[]]; });
        }];
        return;
    }
}
- (void)addTyped {
    NSString *t = _address.text ?: @"";
    if (!t.length) { [_address becomeFirstResponder]; return; }
    _address.text = @""; [_address resignFirstResponder];
    [self addFriends:parse_invites(t)];
}
- (BOOL)textFieldShouldReturn:(UITextField *)f { (void)f; [self addTyped]; return NO; }
- (void)removeFriend:(UIButton *)b {
    NSString *a = _removeAddr[@(b.tag)]; if (!a) return;
    NSMutableArray *list = friends_list().mutableCopy;
    [list filterUsingPredicate:[NSPredicate predicateWithBlock:^BOOL(NSDictionary *o, NSDictionary *bind) { (void)bind; return ![o[@"a"] isEqualToString:a]; }]];
    friends_save(list); apply(); [self refresh];
}
- (void)flash:(NSString *)msg {   // a short line under the capsule
    UILabel *l = label(msg, 13, UIFontWeightMedium, UIColor.whiteColor);
    l.backgroundColor = [UIColor colorWithWhite:0.11 alpha:0.92]; l.layer.cornerRadius = 12; l.clipsToBounds = YES; l.textAlignment = NSTextAlignmentCenter;
    CGSize s = [l sizeThatFits:CGSizeMake(self.bounds.size.width - 40, 30)];
    CGFloat w = MIN(s.width + 28, self.bounds.size.width - 40);
    l.frame = CGRectMake(floor((self.bounds.size.width - w) / 2), CGRectGetMaxY(_pill.frame) + (self.open ? CGRectGetHeight(_panel.frame) + 16 : 8), w, 28);
    l.alpha = 0; [self addSubview:l];
    [UIView animateWithDuration:0.2 animations:^{ l.alpha = 1; } completion:^(BOOL d) {
        (void)d; [UIView animateWithDuration:0.3 delay:2.5 options:0 animations:^{ l.alpha = 0; } completion:^(BOOL d2) { (void)d2; [l removeFromSuperview]; }];
    }];
}
@end

static DmLanOverlay *g_overlay;
static UIWindow *key_window(void) {
    for (UIScene *s in UIApplication.sharedApplication.connectedScenes) {
        if (![s isKindOfClass:UIWindowScene.class]) continue;
        UIWindowScene *ws = (UIWindowScene *)s;
        for (UIWindow *w in ws.windows) if (w.isKeyWindow) return w;
        return ws.windows.firstObject;
    }
    return nil;
}
// The game's local wireless state (lan_core.h): shown while it hosts or looks (AccessPoint .. StationConnected). Hidden only once it has stayed out of those for
// two seconds: Mario Kart 8 Deluxe closes and opens its station several times a second while it lists rooms, and each close hid the overlay (and the keyboard
// of a setting being typed)
static uint64_t g_hide_gen;
static int g_last_state = -1;
void lan_ui_state(int state) {
    int show = state >= LDN_AP && state <= LDN_STATION_CONNECTED;
    dispatch_async(dispatch_get_main_queue(), ^{
        uint64_t gen = ++g_hide_gen;   // any newer state cancels a pending hide
        if (show && !g_overlay) {
            UIWindow *w = key_window(); if (!w) return;
            g_overlay = [[DmLanOverlay alloc] initWithFrame:w.bounds];
            g_overlay.autoresizingMask = UIViewAutoresizingFlexibleWidth | UIViewAutoresizingFlexibleHeight;
            g_overlay.alpha = 0;
            [w addSubview:g_overlay];
            [UIView animateWithDuration:0.25 animations:^{ g_overlay.alpha = 1; }];
            [g_overlay dim];
        } else if (show && g_overlay) {
            if (state != g_last_state && (state == LDN_AP_CREATED || state == LDN_STATION_CONNECTED)) [g_overlay wake];   // a room opened or joined: shown again for a moment
            [g_overlay refresh];
        } else if (!show && g_overlay) {
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
                if (gen != g_hide_gen || !g_overlay || g_overlay.open) return;   // local play is on again, or the panel is open
                DmLanOverlay *o = g_overlay; g_overlay = nil;
                [UIView animateWithDuration:0.25 animations:^{ o.alpha = 0; } completion:^(BOOL done) { (void)done; [o removeFromSuperview]; }];
            });
        }
        g_last_state = state;
    });
}
void lan_ui_blocked(void) {   // lan_nw.m: iOS denied Local Network access; the capsule says so, as long as the game runs
    if (g_blocked) return;
    g_blocked = 1;
    dispatch_async(dispatch_get_main_queue(), ^{ if (g_overlay) { [g_overlay refresh]; g_overlay.alpha = 1; } });
}
void lan_ui_config(lan_nw_config *out) {   // read once, when local play starts; UserDefaults may be read from any thread, but a game thread has no pool of its own
    @autoreleasepool { read_config(out); }
}
#else
void lan_ui_state(int state) { (void)state; }
void lan_ui_blocked(void) {}
void lan_ui_config(lan_nw_config *c) {
    const char *e;
    memset(c, 0, sizeof *c);
    snprintf(c->password, sizeof c->password, "%s", (e = getenv("DM_LAN_PASSWORD")) ? e : "");
    snprintf(c->room, sizeof c->room, "%s", (e = getenv("DM_LAN_ROOM")) ? e : "");
    c->port = (e = getenv("DM_LAN_PORT")) ? atoi(e) : LAN_DEFAULT_PORT;
    if (c->port <= 0 || c->port > 65535) c->port = LAN_DEFAULT_PORT;
    c->nearby = !((e = getenv("DM_LAN_NEARBY")) && !strcmp(e, "0"));
    c->vpn = !((e = getenv("DM_LAN_VPN")) && !strcmp(e, "0"));
    c->friends = !((e = getenv("DM_LAN_FRIENDS")) && !strcmp(e, "0"));
    if ((e = getenv("DM_LAN_PEERS"))) {   // "host[:port]", separated by commas or spaces
        char buf[1024]; snprintf(buf, sizeof buf, "%s", e);
        for (char *save = NULL, *t = strtok_r(buf, ", \n\t;", &save); t && c->nfriends < LAN_FRIENDS_MAX; t = strtok_r(NULL, ", \n\t;", &save))
            snprintf(c->friend_list[c->nfriends++].addr, sizeof c->friend_list[0].addr, "%s", t);
    }
}
#endif
