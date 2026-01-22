import sys
import time
import os
import threading
import requests
import json
import asyncio
import websockets
import ctypes
import logging
from collections import deque
from PyQt5.QtWidgets import QApplication, QWidget, QLabel, QHBoxLayout, QMenu, QAction, QVBoxLayout, QInputDialog, QSystemTrayIcon
from PyQt5.QtCore import Qt, QTimer, QPoint, QPropertyAnimation, QEasingCurve, QRect, pyqtSignal
from PyQt5.QtGui import QColor, QFont, QIcon

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s - %(name)s - %(levelname)s - %(message)s',
    datefmt='%Y-%m-%d %H:%M:%S'
)
logger = logging.getLogger(__name__)

class DynamicIsland(QWidget):
    billing_signal = pyqtSignal(str, float)
    
    def __init__(self):
        super().__init__()
        self.anim = None
        self.initial_geometry = None
        self.billing_queue = deque()
        self.is_busy = False
        self.currency = "CNY"  # 默认人民币
        self.currency_symbol = "￥"
        self.initUI()
        self.initSystemTray()
        self.oldPos = self.pos()
        self.last_processed_id = None
        self.billing_signal.connect(self.show_billing)
        self.ws = None
        self.ws_thread = None
        self.is_shrunk = False  # 是否收缩到小圆点
        self.shrink_anim = None  # 收缩动画

    def initUI(self):
        self.setWindowFlags(Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint | Qt.Tool)
        self.setAttribute(Qt.WA_TranslucentBackground)
        
        # 静默模式：默认隐藏窗口（调试模式：直接显示）
        # self.hide()
        self.show()
        
        self.container = QWidget(self)
        self.container.setObjectName("IslandContainer")
        self.container.setFixedSize(350, 50)
        
        layout = QHBoxLayout(self.container)
        layout.setContentsMargins(20, 0, 20, 0)
        layout.setSpacing(10)

        self.label_model = QLabel("Sentinel")
        self.label_model.setStyleSheet("color: #888; font-weight: bold; font-size: 10pt;")
        
        self.label_cost = QLabel(f"{self.currency_symbol}0.0000")
        self.label_cost.setStyleSheet("color: #00FF7F; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")

        layout.addWidget(self.label_model)
        layout.addStretch()
        layout.addWidget(self.label_cost)

        self.container.setStyleSheet(""" 
            #IslandContainer { 
                background-color: rgb(15, 15, 20); 
                border: 1px solid rgba(255, 255, 255, 0.1); 
                border-radius: 25px; 
            } 
        """)

        main_layout = QVBoxLayout(self)
        main_layout.setContentsMargins(0, 0, 0, 0)
        main_layout.addWidget(self.container, 0, Qt.AlignCenter)

    def initSystemTray(self):
        self.tray_icon = QSystemTrayIcon(self)
        self.tray_icon.setToolTip("Deep Sentinel - 哨兵")
        
        # 创建托盘菜单
        tray_menu = QMenu()
        
        show_action = QAction("显示窗口", self)
        show_action.triggered.connect(self.show_window)
        tray_menu.addAction(show_action)
        
        set_limit_action = QAction("设置限额", self)
        set_limit_action.triggered.connect(self.show_limit_dialog)
        tray_menu.addAction(set_limit_action)
        
        quit_action = QAction("退出", self)
        quit_action.triggered.connect(QApplication.instance().quit)
        tray_menu.addAction(quit_action)
        
        self.tray_icon.setContextMenu(tray_menu)
        
        # 点击托盘图标显示/隐藏窗口
        self.tray_icon.activated.connect(self.on_tray_activated)
        
        self.tray_icon.show()

    async def connect_websocket(self):
        uri = "ws://127.0.0.1:3001/v1/ws"
        logger.info(f"🔌 [WebSocket] 正在连接到 {uri}...")
        
        try:
            async with websockets.connect(uri) as websocket:
                logger.info(f"✅ [WebSocket] 连接成功！")
                self.ws = websocket
                logger.info(f"🔍 [WebSocket] 开始监听消息...")
                async for message in websocket:
                    print(f"📨 [WebSocket] 收到原始消息: {message[:100]}...")
                    try:
                        data = json.loads(message)
                        print(f"🔍 [WebSocket] 解析后的数据: {data}")
                        
                        # ✅ 统一处理逻辑：优先检查 type 字段
                        if data.get("type") == "billing":
                            model = data.get("model", "Unknown")
                            cost = data.get("cost", 0.0)
                            
                            # 更新币种符号
                            if "currency" in data:
                                self.currency = data["currency"]
                                self.currency_symbol = "$" if data["currency"] == "USD" else "￥"
                                print(f"🌍 [WebSocket] 币种更新为: {self.currency} ({self.currency_symbol})")
                            
                            # 🆕 [铁血熔断] 检查是否是熔断信号
                            if data.get("fused", False):
                                print(f"🚨 [WebSocket] 收到熔断信号！准备传递给UI")
                                self.billing_signal.emit(data, cost)
                            else:
                                print(f"💰 [WebSocket] 收到计费: {model} = {self.currency_symbol}{cost:.6f}")
                                print(f"🚀 [WebSocket] 准备触发 billing_signal.emit()")
                                self.billing_signal.emit(model, cost)
                                print(f"✅ [WebSocket] billing_signal.emit() 已完成")
                        elif data.get("type") == "error":
                            # 🆕 [熔断错误] 处理错误信号
                            reason = data.get("reason", "unknown")
                            if reason == "budget_exceeded":
                                print(f"🚨 [WebSocket] 收到预算超支错误信号！")
                                cost = data.get("cost", 0.0)
                                self.show_billing({"fused": True, "reason": reason}, cost)
                        else:
                            print(f"⚠️ [WebSocket] 收到未知类型消息: {data.get('type', 'N/A')}")
                    except Exception as e:
                        print(f"❌ [WebSocket] 消息解析失败: {e}")
                        import traceback
                        traceback.print_exc()
        except Exception as e:
            print(f"❌ [WebSocket] 连接失败: {e}")
            print(f"🔄 [WebSocket] 5秒后重连...")
            await asyncio.sleep(5)
            await self.connect_websocket()

    def start_websocket_thread(self):
        if self.ws_thread is None or not self.ws_thread.is_alive():
            self.ws_thread = threading.Thread(target=self._run_websocket, daemon=True)
            self.ws_thread.start()
            print(f"🚀 [WebSocket] 线程已启动")

    def _run_websocket(self):
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        try:
            loop.run_until_complete(self.connect_websocket())
        except Exception as e:
            print(f"❌ [WebSocket] 线程异常: {e}")
        finally:
            loop.close()

    def on_tray_activated(self, reason):
        if reason == QSystemTrayIcon.Trigger:
            if self.isVisible():
                self.hide()
            else:
                self.show_window()

    def show_window(self):
        self.show()
        self.setWindowOpacity(1.0)
        if self.is_shrunk:
            self.expand_from_dot()

    def shrink_to_dot(self):
        if self.is_shrunk:
            return
            
        self.is_shrunk = True
        curr = self.geometry()
        center_x = curr.x() + curr.width() // 2
        center_y = curr.y() + curr.height() // 2
        dot_size = 30
        
        if self.shrink_anim and self.shrink_anim.state() == QPropertyAnimation.Running:
            self.shrink_anim.stop()
        
        self.shrink_anim = QPropertyAnimation(self, b"geometry")
        self.shrink_anim.setDuration(300)
        self.shrink_anim.setEasingCurve(QEasingCurve.InOutQuad)
        self.shrink_anim.setEndValue(QRect(center_x - dot_size // 2, center_y - dot_size // 2, dot_size, dot_size))
        self.shrink_anim.start()
        
        # 不自动隐藏窗口，保持一直显示
        # QTimer.singleShot(350, self.hide)

    def expand_from_dot(self):
        if not self.is_shrunk:
            return
            
        self.is_shrunk = False
        curr = self.geometry()
        center_x = curr.x() + curr.width() // 2
        center_y = curr.y() + curr.height() // 2
        new_width = 350
        new_height = 50
        
        if self.shrink_anim and self.shrink_anim.state() == QPropertyAnimation.Running:
            self.shrink_anim.stop()
        
        self.shrink_anim = QPropertyAnimation(self, b"geometry")
        self.shrink_anim.setDuration(300)
        self.shrink_anim.setEasingCurve(QEasingCurve.OutBack)
        self.shrink_anim.setEndValue(QRect(center_x - new_width // 2, center_y - new_height // 2, new_width, new_height))
        self.shrink_anim.start()

    def contextMenuEvent(self, event):
        menu = QMenu(self)
        
        set_limit_action = menu.addAction("设置熔断限额")
        set_limit_action.triggered.connect(self.show_limit_dialog)
        
        history_chat_action = menu.addAction("📚 历史对话")
        history_chat_action.triggered.connect(self.show_history_info)
        
        reset_cost_action = menu.addAction("💰 重置费用")
        reset_cost_action.triggered.connect(self.reset_cost)
        
        quit_action = menu.addAction("退出哨兵")
        quit_action.triggered.connect(QApplication.instance().quit)
        
        menu.exec_(event.globalPos())

    def show_limit_dialog(self):
        currency_name = "美元" if self.currency == "USD" else "人民币"
        amount, ok = QInputDialog.getDouble(
            self,
            "设置熔断金额",
            f"请输入最大消费限额（{currency_name}）：",
            0.01,
            0,
            100,
            4
        )
        
        if ok:
            try:
                print(f"🔄 [UI] 用户更新限额: {self.currency_symbol}{amount}")
                
                response = requests.post(
                    "http://127.0.0.1:3001/v1/config/limit",
                    json={"limit": amount},
                    timeout=5
                )
                
                if response.status_code == 200:
                    print(f"✅ [UI] 限额更新成功: {self.currency_symbol}{amount}")
                    self.show_billing("限额已更新", amount)
                else:
                    print(f"❌ [UI] 限额更新失败: {response.status_code}")
            except Exception as e:
                print(f"❌ [UI] 限额更新异常: {e}")
    
    def start_new_chat(self):
        new_session_id = f"session_{int(time.time())}"
        print(f"🧹 [UI] 开启新对话: {new_session_id}")
        
        # 🆕 [新对话] 灵动岛闪绿光
        self.show()
        self.setWindowOpacity(1.0)
        self.label_model.setText("🧹 新对话")
        self.label_model.setStyleSheet("color: #52C41A; font-weight: bold;")
        self.label_cost.setText(f"ID: {new_session_id[-8:]}")
        self.label_cost.setStyleSheet("color: #52C41A; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")
        self.container.setStyleSheet(""" 
            #IslandContainer { 
                background-color: rgba(82, 196, 26, 0.95); 
                border: 2px solid rgba(255, 255, 255, 0.5); 
                border-radius: 25px; 
            } 
        """)
        
        # 3秒后恢复默认样式
        QTimer.singleShot(3000, self.reset_style)
    
    def reset_cost(self):
        print(f"💰 [UI] 重置累计费用")
        
        try:
            response = requests.post(
                "http://127.0.0.1:3001/v1/config/reset_cost",
                json={},
                timeout=5
            )
            
            if response.status_code == 200:
                print(f"✅ [UI] 费用重置成功")
                self.show_reset_success()
            else:
                print(f"❌ [UI] 费用重置失败: {response.status_code}")
                self.show_error_message("重置失败", "无法重置累计费用")
        except Exception as e:
            print(f"❌ [UI] 费用重置异常: {e}")
            self.show_error_message("重置失败", str(e))
    
    def show_reset_success(self):
        print(f"✅ [UI] 显示重置成功提示")
        
        # 🆕 [重置成功] 灵动岛闪绿光
        self.show()
        self.setWindowOpacity(1.0)
        self.label_model.setText("💰 费用已重置")
        self.label_model.setStyleSheet("color: #4CAF50; font-weight: bold;")
        self.label_cost.setText("从 0 开始")
        self.label_cost.setStyleSheet("color: #4CAF50; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")
        self.container.setStyleSheet(""" 
            #IslandContainer { 
                background-color: rgba(76, 175, 80, 0.95); 
                border: 2px solid rgba(255, 255, 255, 0.5); 
                border-radius: 25px; 
            } 
        """)
        
        # 3秒后恢复默认样式
        QTimer.singleShot(3000, self.reset_style)
        
        # 💡 [重置成功] 提示用户
        print(f"💡 [UI] 提示：累计费用已重置为 0，可以重新开始计费！")
        
        # 💡 [新对话] 提示用户
        print(f"💡 [UI] 新对话已创建，下次请求将使用新 session_id: {new_session_id}")
        print(f"💰 [UI] 提示：新对话不会继承历史记录，可以节省 Token 成本！")
    
    def show_history_info(self):
        print(f"📚 [UI] 显示历史对话信息")
        
        # 🆕 [历史对话] 从 Redis DB1 获取所有历史对话
        try:
            import subprocess
            result = subprocess.run(
                [".\\Redis\\redis-cli.exe", "-p", "6379", "-n", "1", "KEYS", "sentinel:chat:*"],
                capture_output=True,
                text=True,
                cwd="d:\\Deep-Sentinel"
            )
            
            if result.returncode == 0:
                keys = result.stdout.strip().split('\n')
                session_ids = [key.replace('sentinel:chat:', '') for key in keys if key.strip()]
                
                if not session_ids:
                    print(f"⚠️ [UI] 没有找到历史对话")
                    self.show_no_history_warning()
                    return
                
                print(f"📚 [UI] 找到 {len(session_ids)} 个历史对话")
                
                # 显示历史对话列表
                session_id, ok = QInputDialog.getItem(
                    self,
                    "选择历史对话",
                    "请选择要继续的对话：",
                    session_ids,
                    0,  # 默认选择第一个
                    False  # 不允许编辑
                )
                
                if ok and session_id:
                    print(f"📚 [UI] 用户选择了历史对话: {session_id}")
                    self.show_selected_history(session_id)
            else:
                print(f"❌ [UI] 获取历史对话失败: {result.stderr}")
                self.show_error_message("获取历史对话失败", "无法连接到 Redis 数据库")
        except Exception as e:
            print(f"❌ [UI] 获取历史对话异常: {e}")
            self.show_error_message("获取历史对话失败", str(e))
    
    def show_selected_history(self, session_id):
        print(f"📚 [UI] 显示选中的历史对话: {session_id}")
        
        # 🆕 [历史对话] 灵动岛闪蓝光
        self.show()
        self.setWindowOpacity(1.0)
        self.label_model.setText("📚 历史对话")
        self.label_model.setStyleSheet("color: #2196F3; font-weight: bold;")
        self.label_cost.setText(f"ID: {session_id[-8:]}")
        self.label_cost.setStyleSheet("color: #2196F3; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")
        self.container.setStyleSheet(""" 
            #IslandContainer { 
                background-color: rgba(33, 150, 243, 0.95); 
                border: 2px solid rgba(255, 255, 255, 0.5); 
                border-radius: 25px; 
            } 
        """)
        
        # 3秒后恢复默认样式
        QTimer.singleShot(3000, self.reset_style)
        
        # 💡 [历史对话] 提示用户
        print(f"💡 [UI] 已选择历史对话: {session_id}")
        print(f"💰 [UI] 提示：下次请求将使用历史对话的 session_id，会继承所有历史记录！")
    
    def show_no_history_warning(self):
        print(f"⚠️ [UI] 显示没有历史对话警告")
        
        # 🆕 [警告] 灵动岛闪黄光
        self.show()
        self.setWindowOpacity(1.0)
        self.label_model.setText("⚠️ 无历史")
        self.label_model.setStyleSheet("color: #FFA500; font-weight: bold;")
        self.label_cost.setText("暂无记录")
        self.label_cost.setStyleSheet("color: #FFA500; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")
        self.container.setStyleSheet(""" 
            #IslandContainer { 
                background-color: rgba(255, 165, 0, 0.95); 
                border: 2px solid rgba(255, 255, 255, 0.5); 
                border-radius: 25px; 
            } 
        """)
        
        # 3秒后恢复默认样式
        QTimer.singleShot(3000, self.reset_style)
        
        # 💡 [警告] 提示用户
        print(f"💡 [UI] 提示：当前没有历史对话记录")
    
    def show_error_message(self, title, message):
        print(f"❌ [UI] 显示错误消息: {title} - {message}")
        
        # 🆕 [错误] 灵动岛闪红光
        self.show()
        self.setWindowOpacity(1.0)
        self.label_model.setText("❌ 错误")
        self.label_model.setStyleSheet("color: #F44336; font-weight: bold;")
        self.label_cost.setText(message)
        self.label_cost.setStyleSheet("color: #F44336; font-family: 'Consolas'; font-size: 12pt; font-weight: bold;")
        self.container.setStyleSheet(""" 
            #IslandContainer { 
                background-color: rgba(244, 67, 54, 0.95); 
                border: 2px solid rgba(255, 255, 255, 0.5); 
                border-radius: 25px; 
            } 
        """)
        
        # 3秒后恢复默认样式
        QTimer.singleShot(3000, self.reset_style)
    
    def reset_style(self):
        self.container.setStyleSheet(""" 
            #IslandContainer { 
                background-color: rgba(255, 255, 255, 0.9); 
                border: 2px solid rgba(255, 255, 255, 0.5); 
                border-radius: 25px; 
            } 
        """)
        self.label_model.setStyleSheet("color: #333; font-weight: bold;")
        self.label_cost.setStyleSheet("color: #333; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")

    def mousePressEvent(self, event):
        self.oldPos = event.globalPos()

    def mouseMoveEvent(self, event):
        delta = QPoint(event.globalPos() - self.oldPos)
        self.move(self.x() + delta.x(), self.y() + delta.y())
        self.oldPos = event.globalPos()

    def show_billing(self, model, price):
        print(f"🎬 [UI] show_billing 被调用: 模型={model}, 价格={self.currency_symbol}{price}")
        
        # 🆕 [铁血熔断] 检查是否是熔断信号
        if isinstance(model, dict):
            # 处理熔断信号（包含fused字段或reason字段）
            if model.get("fused", False) or model.get("reason") == "budget_exceeded":
                print(f"🚨 [UI] 收到熔断信号！")
                self.show()
                self.setWindowOpacity(1.0)
                
                self.label_model.setText("🚨 熔断拦截")
                self.label_model.setStyleSheet("color: #FF4D4F; font-weight: bold;")
                
                self.label_cost.setText(f"{self.currency_symbol}{price:.4f}")
                self.label_cost.setStyleSheet("color: #FF4D4F; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")
                
                self.container.setStyleSheet(""" 
                    #IslandContainer { 
                        background-color: rgba(255, 77, 79, 0.95); 
                        border: 2px solid rgba(255, 255, 255, 0.5); 
                        border-radius: 25px; 
                    } 
                """)
            return
        
        # ✅ 过滤无效计费：只有价格大于 0.000001 才显示
        if price <= 0.000001:
            print(f"🚫 [Island] 收到无效计费 {self.currency_symbol}{price:.8f}，已忽略")
            return
        
        # 🚀 [绝对实时] 直接更新显示，不使用队列和动画
        self.show()
        self.setWindowOpacity(1.0)
        
        self.label_model.setText(model)
        self.label_model.setStyleSheet("color: #888; font-weight: bold;")
        
        # 根据金额大小决定显示精度
        precision = 6 if price < 0.01 else 4
        self.label_cost.setText(f"{self.currency_symbol}{price:.{precision}f}")
        
        if price > 0.008:
            self.container.setStyleSheet(""" 
                #IslandContainer { 
                    background-color: rgba(255, 68, 68, 0.95); 
                    border: 1px solid rgba(255, 255, 255, 0.3); 
                    border-radius: 25px; 
                } 
            """)
            self.label_cost.setStyleSheet("color: #FFFFFF; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")
        elif price > 0.005:
            self.container.setStyleSheet(""" 
                #IslandContainer { 
                    background-color: rgba(255, 136, 0, 0.95); 
                    border: 1px solid rgba(255, 255, 255, 0.3); 
                    border-radius: 25px; 
                } 
            """)
            self.label_cost.setStyleSheet("color: #FFFFFF; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")
        else:
            self.container.setStyleSheet(""" 
                #IslandContainer { 
                    background-color: rgba(15, 15, 20, 0.95); 
                    border: 1px solid rgba(255, 255, 255, 0.2); 
                    border-radius: 25px; 
                } 
            """)
            self.label_cost.setStyleSheet("color: #00FF7F; font-family: 'Consolas'; font-size: 14pt; font-weight: bold;")

    def _reset_style(self):
        print(f"🔄 [UI] 重置样式")
        self.setStyleSheet(""" 
            #IslandContainer { 
                background-color: qlineargradient(x1:0, y1:0, x2:1, y2:1, 
                                    stop:0 rgba(20, 20, 25, 240), 
                                    stop:1 rgba(10, 10, 15, 250)); 
                border: 1px solid rgba(255,255, 255, 0.1); 
                border-radius: 25px; 
            } 
            QLabel { 
                background: transparent; 
            } 
        """)

    def _display_billing(self, model, price):
        print(f"🎬 [UI] 开始显示: 模型={model}, 价格={self.currency_symbol}{price}")
        self.label_model.setText(model)
        self.label_model.setStyleSheet("color: #FFFFFF; font-weight: bold;")
        
        # 根据金额大小决定显示精度
        precision = 6 if price < 0.01 else 4
        self.label_cost.setText(f"{self.currency_symbol}{price:.{precision}f}")
        print(f"🎬 [UI] 标签已更新，开始动画...")

        if self.anim and self.anim.state() == QPropertyAnimation.Running:
            self.anim.stop()

        self.anim = QPropertyAnimation(self, b"geometry")
        self.anim.setDuration(300)
        self.anim.setEasingCurve(QEasingCurve.OutBack)

        curr = self.geometry()
        print(f"🎬 [UI] 当前窗口位置: {curr}")
        new_width = 350
        center_x = curr.x() + curr.width() // 2
        new_x = center_x - new_width // 2
        
        self.anim.setEndValue(QRect(new_x, curr.y(), new_width, 50))
        self.anim.start()
        print(f"🎬 [UI] 动画已启动，目标宽度: {new_width}px，新位置: ({new_x}, {curr.y()})")
        
        # 常驻模式：不自动收缩，保持窗口一直显示

        if self.billing_queue:
            next_model, next_price = self.billing_queue.popleft()
            print(f"📋 [队列] 从队列取出下一个: {next_model}")
            print(f"📋 [队列] 剩余队列长度: {len(self.billing_queue)}")
            QTimer.singleShot(100, lambda: self._display_billing(next_model, next_price))
        else:
            print(f"📋 [队列] 队列为空，解除忙碌状态")
            self.is_busy = False

def start_websocket_thread(window):
    print(f"🚀 [DEBUG] 启动 WebSocket 线程...")
    window.start_websocket_thread()

def check_single_instance():
    try:
        mutex = ctypes.windll.kernel32.CreateMutexW(None, False, "DeepSentinel_SingleInstance_Mutex")
        if ctypes.windll.kernel32.GetLastError() == 183:
            return False
        return True
    except:
        return True

if __name__ == '__main__':
    if not check_single_instance():
        print("❌ [单例检测] 哨兵已在运行，请勿重复启动")
        sys.exit(1)
    
    app = QApplication(sys.argv)
    ex = DynamicIsland()
    start_websocket_thread(ex)
    sys.exit(app.exec_())
