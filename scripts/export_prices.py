import redis
import json
from datetime import datetime

def export_redis_prices():
    r = redis.Redis(host='127.0.0.1', port=6379, decode_responses=True)
    
    keys = r.keys('price:*')
    
    prices = {}
    
    for key in keys:
        try:
            data = r.get(key)
            if data:
                prices[key] = json.loads(data)
        except Exception as e:
            print(f"Error reading {key}: {e}")
    
    output_file = 'prices_export.json'
    
    with open(output_file, 'w', encoding='utf-8') as f:
        json.dump(prices, f, ensure_ascii=False, indent=2)
    
    print(f"✅ 已导出 {len(prices)} 个模型价格到 {output_file}")
    
    # 按 vendor 分类统计
    vendor_stats = {}
    for key, price_data in prices.items():
        vendor = price_data.get('vendor', 'unknown')
        if vendor not in vendor_stats:
            vendor_stats[vendor] = []
        model_id = key.replace('price:', '')
        vendor_stats[vendor].append({
            'model': model_id,
            'input_price': price_data.get('input_price', 0),
            'output_price': price_data.get('output_price', 0)
        })
    
    print(f"\n📊 价格统计（按 vendor 分类）：")
    print("=" * 80)
    
    for vendor, models in sorted(vendor_stats.items()):
        print(f"\n🏷️  Vendor: {vendor}")
        print(f"   模型数量: {len(models)}")
        print(f"   {'模型':<30} {'输入价格':<15} {'输出价格':<15}")
        print(f"   {'-'*30} {'-'*15} {'-'*15}")
        
        for model_info in sorted(models, key=lambda x: x['model']):
            model = model_info['model']
            input_price = model_info['input_price']
            output_price = model_info['output_price']
            print(f"   {model:<30} {input_price:<15.6f} {output_price:<15.6f}")
    
    print(f"\n{'='*80}")
    print(f"📝 总计: {len(prices)} 个模型")
    print(f"📝 Vendor 数量: {len(vendor_stats)} 个")
    
    return prices

if __name__ == '__main__':
    export_redis_prices()
