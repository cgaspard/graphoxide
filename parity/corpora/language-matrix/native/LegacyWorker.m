#import "LegacyWorker.h"

@implementation LegacyWorker
- (NSString *)process:(NSString *)value {
    return [value stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceCharacterSet];
}
@end
