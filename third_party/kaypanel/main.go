package main

import (
	"bufio"
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	"kaypanel/internal/app"
	"kaypanel/web"
)

var version = "dev"

// handshake 是 sidecar 模式下向父进程回报的唯一一行 stdout 输出。
// token 由本进程内部随机生成，只经这条私有管道交给父进程，
// 不出现在命令行参数或环境变量里，避免被同用户的其它进程读到。
type handshake struct {
	URL   string `json:"url"`
	Token string `json:"token"`
	PID   int    `json:"pid"`
}

func main() {
	configPath := flag.String("config", "data/config.yaml", "配置文件路径")
	listen := flag.String("listen", "", "监听地址，覆盖配置文件")
	showVersion := flag.Bool("version", false, "显示版本")
	sidecar := flag.Bool("sidecar", false, "sidecar 模式：监听回环随机端口，stdout 输出握手 JSON，stdin 关闭即退出")
	flag.Parse()
	if *showVersion {
		fmt.Printf("kaypanel %s\n", version)
		return
	}

	// sidecar 模式永远只听回环随机端口，忽略配置文件里的监听地址，
	// 避免用户误把面板暴露到局域网。
	if *sidecar {
		*listen = "127.0.0.1:0"
	}

	inst, err := app.Start(app.Options{
		ConfigPath: *configPath,
		Listen:     *listen,
		Version:    version,
		Web:        web.Assets,
		// 桌面/嵌入模式：启用一次性桌面登录令牌，父进程用它免密进面板。
		Desktop: *sidecar,
	})
	if err != nil {
		log.Fatal(err)
	}
	if inst.InitialPassword != "" {
		log.Printf("==============================================")
		log.Printf(" 首次启动，管理员密码: %s", inst.InitialPassword)
		log.Printf(" 登录后请在「设置」中修改密码")
		log.Printf("==============================================")
	}
	log.Printf("KayPanel %s listening on %s", version, inst.Addr)

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt, syscall.SIGTERM)

	if *sidecar {
		line, err := json.Marshal(handshake{
			URL:   inst.URL,
			Token: inst.DesktopToken,
			PID:   os.Getpid(),
		})
		if err != nil {
			log.Fatalf("握手信息编码失败: %v", err)
		}
		// 只有这一行写 stdout，父进程按行读取；日志全部走 stderr。
		fmt.Printf("%s\n", line)
		// stdin 是父进程给的管道：父进程退出、崩溃或主动关闭时读到 EOF。
		// Windows 不支持 SIGTERM，所以这里直接向 stop 投递，走正常关闭流程。
		go func() {
			_, _ = bufio.NewReader(os.Stdin).ReadString('\n')
			select {
			case stop <- os.Interrupt:
			default:
			}
		}()
	}

	<-stop
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	inst.Shutdown(ctx)
	log.Println("已退出")
}
