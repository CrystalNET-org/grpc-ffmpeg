### Building the Project

#### 1. Clone the repository:

```bash
git clone https://github.com/CrystalNET-org/grpc-ffmpeg.git
cd grpc-ffmpeg
```

#### 2. Install Python dependencies:

```bash
pip install -r requirements.txt
```

#### 3. Compile the Protobuf definition:

```bash
python -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. proto/ffmpeg.proto
```